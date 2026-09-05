use chrono::{DateTime, Duration, TimeZone, Utc};
use rand::RngExt;
use sea_orm::sea_query::{Alias, Expr, Func, OnConflict};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter, QueryOrder,
    QuerySelect, Set,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::error::Error;

use crate::plugins::ciyi::config::CiYiConfig;
use crate::plugins::ciyi::data::{get_all_words, get_question_words};
use crate::plugins::ciyi::entity::{record, state};
use crate::plugins::ciyi::view::{Board, HintRow, RankBoard, RankItem, Reply, Win};

/// 一条提示。
///
/// `text` 是它的经典文本形态，历史存档只有这一列；`word` / `prev` / `next`
/// 是后来为卡片排版补的结构化字段，都带 `serde(default)`，
/// 老存档读回来是 `None`，此时从 `text` 反解，不必迁移数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hint {
    pub text: String,
    pub rank: usize,
    #[serde(default)]
    pub word: Option<String>,
    #[serde(default)]
    pub prev: Option<String>,
    #[serde(default)]
    pub next: Option<String>,
}

impl Hint {
    /// 从 `？佩 ) 东西 ( 冥？ #16` 反解出 (左邻末字, 词, 右邻首字)
    fn parse_text(text: &str) -> Option<(String, String, String)> {
        let (left, rest) = text.split_once(" ) ")?;
        let (word, right) = rest.split_once(" ( ")?;
        let prev = left.trim().strip_prefix('？').unwrap_or(left.trim());
        let next = right.trim().chars().next()?;
        Some((prev.to_string(), word.trim().to_string(), next.to_string()))
    }

    /// 摊平成卡片用的一行
    pub fn row(&self, fresh: bool) -> HintRow {
        let parsed = Hint::parse_text(&self.text);
        let pick = |field: &Option<String>, from_parsed: fn(&(String, String, String)) -> &String| {
            field
                .clone()
                .or_else(|| parsed.as_ref().map(|p| from_parsed(p).clone()))
                .unwrap_or_else(|| "？".to_string())
        };
        HintRow {
            rank: self.rank,
            prev: pick(&self.prev, |p| &p.0),
            word: pick(&self.word, |p| &p.1),
            next: pick(&self.next, |p| &p.2),
            fresh,
        }
    }
}

impl Ord for Hint {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank.cmp(&other.rank)
    }
}

impl PartialOrd for Hint {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for Hint {}

impl PartialEq for Hint {
    fn eq(&self, other: &Self) -> bool {
        self.rank == other.rank
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiYiGameState {
    pub group_id: i64,
    pub target_word: String,
    pub last_start_time: DateTime<Utc>,
    pub global_history: HashSet<String>,
    pub current_guesses: HashSet<String>,
    pub words_rank_list: Vec<String>,
    pub hints: Vec<Hint>,
    pub is_finished: bool,
    pub direct_guess_enabled: bool,
}

impl CiYiGameState {
    pub fn is_new_day_in_china_timezone(&self) -> bool {
        const CHINA_TIMEZONE_OFFSET_HOURS: i64 = 8;
        let now_in_china_tz = Utc::now() + Duration::hours(CHINA_TIMEZONE_OFFSET_HOURS);
        let last_start_in_china_tz =
            self.last_start_time + Duration::hours(CHINA_TIMEZONE_OFFSET_HOURS);
        now_in_china_tz.date_naive() != last_start_in_china_tz.date_naive()
    }

    /// 从数据库加载状态
    pub async fn load(
        db: &DatabaseConnection,
        group_id: i64,
    ) -> Result<Option<Self>, Box<dyn Error + Send + Sync>> {
        let model = state::Entity::find_by_id(group_id).one(db).await?;
        if let Some(m) = model {
            Ok(Some(Self {
                group_id: m.group_id,
                target_word: m.target_word,
                last_start_time: Utc.timestamp_opt(m.last_start_time, 0).unwrap(),
                global_history: unsafe {
                    simd_json::from_str(&mut m.global_history.clone()).unwrap_or_default()
                },
                current_guesses: unsafe {
                    simd_json::from_str(&mut m.current_guesses.clone()).unwrap_or_default()
                },
                words_rank_list: unsafe {
                    simd_json::from_str(&mut m.words_rank_list.clone()).unwrap_or_default()
                },
                hints: unsafe { simd_json::from_str(&mut m.hints.clone()).unwrap_or_default() },
                is_finished: m.is_finished,
                direct_guess_enabled: m.direct_guess_enabled,
            }))
        } else {
            Ok(None)
        }
    }

    /// 保存状态到数据库
    pub async fn save(&self, db: &DatabaseConnection) -> Result<(), Box<dyn Error + Send + Sync>> {
        let active_model = state::ActiveModel {
            group_id: Set(self.group_id),
            target_word: Set(self.target_word.clone()),
            last_start_time: Set(self.last_start_time.timestamp()),
            global_history: Set(simd_json::to_string(&self.global_history)?),
            current_guesses: Set(simd_json::to_string(&self.current_guesses)?),
            words_rank_list: Set(simd_json::to_string(&self.words_rank_list)?),
            hints: Set(simd_json::to_string(&self.hints)?),
            is_finished: Set(self.is_finished),
            direct_guess_enabled: Set(self.direct_guess_enabled),
        };

        state::Entity::insert(active_model)
            .on_conflict(
                OnConflict::column(state::Column::GroupId)
                    .update_columns([
                        state::Column::TargetWord,
                        state::Column::LastStartTime,
                        state::Column::GlobalHistory,
                        state::Column::CurrentGuesses,
                        state::Column::WordsRankList,
                        state::Column::Hints,
                        state::Column::IsFinished,
                        state::Column::DirectGuessEnabled,
                    ])
                    .to_owned(),
            )
            .exec(db)
            .await?;

        Ok(())
    }
}

#[derive(Debug)]
pub enum FetchReason {
    NewGame,
    NewDay,
    MissingRankList,
}

#[derive(Debug)]
pub struct FetchRequest {
    pub word_to_fetch: String,
    pub reason: FetchReason,
}

pub struct FetchedData {
    pub request: FetchRequest,
    pub result: Result<Vec<String>, Box<dyn Error + Send + Sync>>,
}

// -------------------------------------------------------------------------
// 核心逻辑函数
// -------------------------------------------------------------------------

pub async fn prepare_guess(
    db: &DatabaseConnection,
    group_id: i64,
) -> Result<Option<FetchRequest>, Box<dyn Error + Send + Sync>> {
    let question_words = get_question_words();
    let state_opt = CiYiGameState::load(db, group_id).await?;

    let state = match state_opt {
        Some(s) => s,
        None => {
            // 如果没有记录，说明是新游戏
            let idx = rand::rng().random_range(0..question_words.len());
            let target = &question_words[idx];
            return Ok(Some(FetchRequest {
                word_to_fetch: target.to_string(),
                reason: FetchReason::NewGame,
            }));
        }
    };

    if state.is_finished && state.is_new_day_in_china_timezone() {
        let candidates: Vec<&str> = question_words
            .iter()
            .filter(|w| !state.global_history.contains(w.as_str()))
            .map(|w| w.as_str())
            .collect();

        if candidates.is_empty() {
            return Ok(None);
        }

        let idx = rand::rng().random_range(0..candidates.len());
        let new_target = candidates[idx].to_string();
        return Ok(Some(FetchRequest {
            word_to_fetch: new_target,
            reason: FetchReason::NewDay,
        }));
    }

    if !state.is_finished && state.words_rank_list.is_empty() {
        return Ok(Some(FetchRequest {
            word_to_fetch: state.target_word.clone(),
            reason: FetchReason::MissingRankList,
        }));
    }

    Ok(None)
}

pub async fn commit_guess(
    db: &DatabaseConnection,
    group_id: i64,
    user_id: i64,
    username: &str,
    guess_word: String,
    fetched_data: Option<FetchedData>,
    config: &CiYiConfig,
) -> Result<Reply, Box<dyn Error + Send + Sync>> {
    let mut state = if let Some(data) = fetched_data {
        let rank_list = match data.result {
            Ok(list) => list,
            Err(e) => return Ok(Reply::Notice(format!("获取词语排名失败：{e}"))),
        };

        // 加载或初始化 State
        let mut s = match CiYiGameState::load(db, group_id).await? {
            Some(existing) => existing,
            None => {
                // Should only happen on NewGame
                CiYiGameState {
                    group_id,
                    target_word: data.request.word_to_fetch.clone(),
                    last_start_time: Utc::now(),
                    global_history: HashSet::new(),
                    current_guesses: HashSet::new(),
                    words_rank_list: Vec::new(),
                    hints: Vec::new(),
                    is_finished: false,
                    direct_guess_enabled: config.plugin.direct_guess,
                }
            }
        };

        match data.request.reason {
            FetchReason::NewGame => {
                s.target_word = data.request.word_to_fetch.clone();
                s.last_start_time = Utc::now();
                s.global_history = HashSet::from([data.request.word_to_fetch.clone()]);
                s.current_guesses = HashSet::new();
                s.words_rank_list = rank_list;
                s.hints = Vec::new();
                s.is_finished = false;
                s.direct_guess_enabled = config.plugin.direct_guess;
            }
            FetchReason::NewDay => {
                s.hints.clear();
                s.current_guesses.clear();
                s.target_word = data.request.word_to_fetch.clone();
                s.global_history.insert(data.request.word_to_fetch);
                s.words_rank_list = rank_list;
                s.last_start_time = Utc::now();
                s.is_finished = false;
            }
            FetchReason::MissingRankList => {
                s.words_rank_list = rank_list;
            }
        }
        // Save initialized/updated state immediately
        s.save(db).await?;
        s
    } else {
        match CiYiGameState::load(db, group_id).await? {
            Some(s) => s,
            None => return Ok(Reply::Notice("游戏尚未开始，请重试。".to_string())),
        }
    };

    if state.is_finished {
        return Ok(Reply::Notice("每天只能玩一次哦！".to_string()));
    }

    // 重复猜测不算错——把盘面重新画一遍，顺带说明这个词猜过了，
    // 比单丢一句「已猜过」有用：想重猜的人多半是想再看一眼当前进度
    if state.current_guesses.contains(&guess_word) {
        return Ok(Reply::Board(build_board(
            &state,
            Some(format!("「{guess_word}」已经猜过了")),
            Some(&guess_word),
            config,
        )));
    }

    // 输入错字是即时纠错，一句话说完就走，不必为它出图
    if !get_all_words().contains(&guess_word) {
        return Ok(Reply::Notice(format!("{guess_word} 不在词库中。")));
    }

    state.current_guesses.insert(guess_word.clone());

    let result = if guess_word == state.target_word {
        state.is_finished = true;

        // 保存赢家记录
        let win_record = record::ActiveModel {
            group_id: Set(group_id),
            user_id: Set(user_id),
            username: Set(username.to_string()),
            timestamp: Set(Utc::now().timestamp()),
            ..Default::default()
        };
        record::Entity::insert(win_record).exec(db).await?;

        Reply::Win(Win {
            answer: state.target_word.clone(),
            winner: username.to_string(),
            guesses: state.current_guesses.len(),
            hits: state.hints.len(),
            started_at: state.last_start_time,
        })
    } else {
        let mut notice = None;
        if let Some(index) = state.words_rank_list.iter().position(|w| w == &guess_word) {
            let rank = index + 1;
            let prev_char = state
                .words_rank_list
                .get(index.wrapping_sub(1))
                .and_then(|w| w.chars().nth(1))
                .unwrap_or('？');
            let next_char = state
                .words_rank_list
                .get(index + 1)
                .and_then(|w| w.chars().next())
                .unwrap_or('？');
            let hint_text = format!("？{prev_char} ) {guess_word} ( {next_char}？ #{rank}");
            state.hints.push(Hint {
                text: hint_text,
                rank,
                word: Some(guess_word.clone()),
                prev: Some(prev_char.to_string()),
                next: Some(next_char.to_string()),
            });
        } else {
            // 词库里有、排名里没有：说明它离答案远到排不进榜，说破比沉默好
            notice = Some(format!("「{guess_word}」未进入排名，离答案很远"));
        }
        state.hints.sort_unstable();
        Reply::Board(build_board(&state, notice, Some(&guess_word), config))
    };

    state.save(db).await?;
    Ok(result)
}

/// 把当前盘面摊平成可渲染的数据：名次升序、截断到配置条数，
/// 并标出本次刚猜的那一条（它可能排在中间，不高亮就找不着）
fn build_board(
    state: &CiYiGameState,
    notice: Option<String>,
    latest: Option<&str>,
    config: &CiYiConfig,
) -> Board {
    let shown = config.plugin.history_display.max(1);
    let rows: Vec<HintRow> = state
        .hints
        .iter()
        .take(shown)
        .map(|hint| {
            let row = hint.row(false);
            let fresh = latest.is_some_and(|word| word == row.word);
            HintRow { fresh, ..row }
        })
        .collect();

    Board {
        notice,
        hidden: state.hints.len().saturating_sub(rows.len()),
        rows,
        guesses: state.current_guesses.len(),
        hits: state.hints.len(),
        pool: state.words_rank_list.len(),
    }
}

pub async fn get_direct_guess_status(
    db: &DatabaseConnection,
    group_id: i64,
    default: bool,
) -> bool {
    if let Ok(Some(state)) = CiYiGameState::load(db, group_id).await {
        (state.is_new_day_in_china_timezone() || !state.is_finished) && state.direct_guess_enabled
    } else {
        default
    }
}

pub async fn toggle_direct_guess_mode(
    db: &DatabaseConnection,
    group_id: i64,
    default: bool,
) -> String {
    let mut state = match CiYiGameState::load(db, group_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            // Initialize empty state if not exists
            let question_words = get_question_words();
            let idx = rand::rng().random_range(0..question_words.len());
            let target = &question_words[idx];
            CiYiGameState {
                group_id,
                target_word: target.to_string(),
                last_start_time: Utc::now(),
                global_history: HashSet::from([target.to_string()]),
                current_guesses: HashSet::new(),
                words_rank_list: Vec::new(),
                hints: Vec::new(),
                is_finished: false,
                direct_guess_enabled: default,
            }
        }
        Err(_) => return "数据库错误。".to_string(),
    };

    state.direct_guess_enabled = !state.direct_guess_enabled;
    let new_status = state.direct_guess_enabled;

    if (state.save(db).await).is_err() {
        return "保存状态失败。".to_string();
    }

    if new_status {
        "直接猜测模式已开启。".to_string()
    } else {
        "直接猜测模式已关闭。".to_string()
    }
}

// -------------------------------------------------------------------------
// 排行榜相关
// -------------------------------------------------------------------------

#[derive(Debug, FromQueryResult)]
struct LeaderboardItem {
    username: String,
    score: i64,
}

pub async fn get_global_leaderboard(db: &DatabaseConnection, limit: usize) -> Reply {
    let results: Vec<LeaderboardItem> = match record::Entity::find()
        .select_only()
        .column(record::Column::UserId)
        .column_as(
            Expr::from(Func::max(Expr::col(record::Column::Username))),
            "username",
        )
        .column_as(
            Expr::from(Func::count(Expr::col(record::Column::Id))),
            "score",
        )
        .group_by(record::Column::UserId)
        .order_by_desc(Expr::custom_keyword(Alias::new("score")))
        .limit(limit as u64)
        .into_model::<LeaderboardItem>()
        .all(db)
        .await
    {
        Ok(r) => r,
        Err(_) => return Reply::Notice("获取排行榜失败。".to_string()),
    };

    rank_board("词意全榜", "全部群聊", results)
}

pub async fn get_channel_leaderboard(
    db: &DatabaseConnection,
    group_id: i64,
    limit: usize,
) -> Reply {
    let results: Vec<LeaderboardItem> = match record::Entity::find()
        .select_only()
        .column(record::Column::UserId)
        .column_as(
            Expr::from(Func::max(Expr::col(record::Column::Username))),
            "username",
        )
        .column_as(
            Expr::from(Func::count(Expr::col(record::Column::Id))),
            "score",
        )
        .filter(record::Column::GroupId.eq(group_id))
        .group_by(record::Column::UserId)
        .order_by_desc(Expr::custom_keyword(Alias::new("score")))
        .limit(limit as u64)
        .into_model::<LeaderboardItem>()
        .all(db)
        .await
    {
        Ok(r) => r,
        Err(_) => return Reply::Notice("获取排行榜失败。".to_string()),
    };

    rank_board("词意榜", "本群", results)
}

fn rank_board(title: &str, scope: &str, items: Vec<LeaderboardItem>) -> Reply {
    let subtitle = match items.len() {
        0 => format!("{scope} · 虚位以待"),
        n => format!("{scope} · 猜中次数前 {n} 名"),
    };
    Reply::Rank(RankBoard {
        title: title.to_string(),
        subtitle,
        items: items
            .into_iter()
            .map(|item| RankItem {
                name: item.username,
                score: item.score,
            })
            .collect(),
    })
}

// -------------------------------------------------------------------------
// 网络请求与入口
// -------------------------------------------------------------------------

pub async fn fetch_words_rank_list(
    word: &str,
) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
    let url = format!("https://ci-ying.oss-cn-zhangjiakou.aliyuncs.com/v1/ci-yi-list/{word}.txt");
    let response = crate::http::get(&url).await?;
    let response = response.error_for_status()?;
    let body_text = response.text().await?;
    let words_rank_list: Vec<String> = body_text
        .trim()
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    Ok(words_rank_list)
}

pub async fn guess_word(
    db: &DatabaseConnection,
    group_id: i64,
    user_id: i64,
    username: &str,
    guess_word: &str,
    config: &CiYiConfig,
) -> Reply {
    let fetch_req = match prepare_guess(db, group_id).await {
        Ok(req) => req,
        Err(e) => return Reply::Notice(format!("系统错误：{}", e)),
    };

    let fetched_data = if let Some(req) = fetch_req {
        let result = fetch_words_rank_list(&req.word_to_fetch).await;
        Some(FetchedData {
            request: req,
            result,
        })
    } else {
        None
    };

    match commit_guess(
        db,
        group_id,
        user_id,
        username,
        guess_word.to_string(),
        fetched_data,
        config,
    )
    .await
    {
        Ok(reply) => reply,
        Err(e) => Reply::Notice(format!("游戏处理错误：{}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(text: &str, rank: usize) -> Hint {
        Hint {
            text: text.to_string(),
            rank,
            word: None,
            prev: None,
            next: None,
        }
    }

    /// 老存档只有 text 一列，卡片仍要能把它拆成三块
    #[test]
    fn legacy_hint_text_is_parsed_back() {
        let row = hint("？佩 ) 东西 ( 冥？ #16", 16).row(true);
        assert_eq!((row.prev.as_str(), row.word.as_str(), row.next.as_str()), ("佩", "东西", "冥"));
        assert_eq!(row.rank, 16);
        assert!(row.fresh);
    }

    /// 排名首尾的邻字本身就是「？」，反解不能把它吃掉
    #[test]
    fn legacy_hint_keeps_unknown_neighbour() {
        let row = hint("？？ ) 东西 ( ？？ #1", 1).row(false);
        assert_eq!((row.prev.as_str(), row.word.as_str(), row.next.as_str()), ("？", "东西", "？"));
    }

    /// 结构化字段在场时直接用，不必解析文本
    #[test]
    fn structured_hint_wins_over_text() {
        let mut h = hint("坏掉的文本", 7);
        h.word = Some("玉佩".into());
        h.prev = Some("子".into());
        h.next = Some("东".into());
        let row = h.row(false);
        assert_eq!((row.prev.as_str(), row.word.as_str(), row.next.as_str()), ("子", "玉佩", "东"));
    }

    /// 文本彻底不可解时退回占位符，不 panic、不丢行
    #[test]
    fn unparsable_hint_degrades_to_placeholders() {
        let row = hint("???", 3).row(false);
        assert_eq!((row.prev.as_str(), row.word.as_str(), row.next.as_str()), ("？", "？", "？"));
    }

    #[test]
    fn board_marks_latest_guess_and_counts_hidden() {
        let mut config = CiYiConfig::default();
        config.plugin.history_display = 2;
        let state = CiYiGameState {
            group_id: 1,
            target_word: "东西".into(),
            last_start_time: Utc::now(),
            global_history: HashSet::new(),
            current_guesses: HashSet::from(["玉佩".to_string(), "镯子".to_string()]),
            words_rank_list: vec!["a".into(); 500],
            hints: vec![
                hint("？器 ) 镯子 ( 玉？ #14", 14),
                hint("？子 ) 玉佩 ( 东？ #15", 15),
                hint("？佩 ) 行头 ( 冥？ #90", 90),
            ],
            is_finished: false,
            direct_guess_enabled: true,
        };

        let board = build_board(&state, None, Some("玉佩"), &config);
        assert_eq!(board.rows.len(), 2, "按 history_display 截断");
        assert_eq!(board.hidden, 1);
        assert_eq!(board.hits, 3);
        assert_eq!(board.pool, 500);
        assert!(!board.rows[0].fresh && board.rows[1].fresh, "只高亮本次猜的词");
    }
}
