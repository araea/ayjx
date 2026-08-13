use chrono::{DateTime, Duration, TimeZone, Utc};
use rand::RngExt;
use sea_orm::sea_query::{Alias, Expr, OnConflict};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hint {
    pub text: String,
    pub rank: usize,
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
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let mut state = if let Some(data) = fetched_data {
        let rank_list = match data.result {
            Ok(list) => list,
            Err(e) => return Ok(format!("获取词语排名失败：{e}")),
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
            None => return Ok("游戏尚未开始，请重试。".to_string()),
        }
    };

    if state.is_finished {
        return Ok("每天只能玩一次哦！".to_string());
    }

    if state.current_guesses.contains(&guess_word) {
        return Ok(format!("{guess_word} 已猜过。"));
    }

    if !get_all_words().contains(&guess_word) {
        return Ok(format!("{guess_word} 不在词库中。"));
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

        format!(
            "恭喜你猜对了！\n答案：{}\n猜测：{} 次",
            state.target_word,
            state.current_guesses.len()
        )
    } else {
        if let Some(index) = state.words_rank_list.iter().position(|w| w == &guess_word) {
            let rank = index + 1;
            let prev_char = state
                .words_rank_list
                .get(index.wrapping_sub(1))
                .and_then(|w| w.chars().nth(1))
                .map_or('？', |c| c);
            let next_char = state
                .words_rank_list
                .get(index + 1)
                .and_then(|w| w.chars().next())
                .map_or('？', |c| c);
            let hint_text = format!("？{prev_char} ) {guess_word} ( {next_char}？ #{rank}");
            state.hints.push(Hint {
                text: hint_text,
                rank,
            });
        }
        state.hints.sort_unstable();
        let hints_str: String = state
            .hints
            .iter()
            .take(config.plugin.history_display)
            .enumerate()
            .map(|(i, hint)| format!("{}. {}\n", i + 1, hint.text))
            .collect();
        format!("{hints_str}...")
    };

    state.save(db).await?;
    Ok(result)
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

pub async fn get_global_leaderboard(db: &DatabaseConnection, limit: usize) -> String {
    let results: Vec<LeaderboardItem> = match record::Entity::find()
        .select_only()
        .column(record::Column::UserId)
        .column_as(Expr::col(record::Column::Username).max(), "username")
        .column_as(Expr::col(record::Column::Id).count(), "score")
        .group_by(record::Column::UserId)
        .order_by_desc(Expr::custom_keyword(Alias::new("score")))
        .limit(limit as u64)
        .into_model::<LeaderboardItem>()
        .all(db)
        .await
    {
        Ok(r) => r,
        Err(_) => return "获取排行榜失败。".to_string(),
    };

    format_leaderboard(results)
}

pub async fn get_channel_leaderboard(
    db: &DatabaseConnection,
    group_id: i64,
    limit: usize,
) -> String {
    let results: Vec<LeaderboardItem> = match record::Entity::find()
        .select_only()
        .column(record::Column::UserId)
        .column_as(Expr::col(record::Column::Username).max(), "username")
        .column_as(Expr::col(record::Column::Id).count(), "score")
        .filter(record::Column::GroupId.eq(group_id))
        .group_by(record::Column::UserId)
        .order_by_desc(Expr::custom_keyword(Alias::new("score")))
        .limit(limit as u64)
        .into_model::<LeaderboardItem>()
        .all(db)
        .await
    {
        Ok(r) => r,
        Err(_) => return "获取排行榜失败。".to_string(),
    };

    format_leaderboard(results)
}

fn format_leaderboard(items: Vec<LeaderboardItem>) -> String {
    if items.is_empty() {
        return "当前还没有人猜对过哦！".to_string();
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| format!("{}. {} {}", index + 1, item.username, item.score))
        .collect::<Vec<String>>()
        .join("\n")
}

// -------------------------------------------------------------------------
// 网络请求与入口
// -------------------------------------------------------------------------

pub async fn fetch_words_rank_list(
    word: &str,
) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
    let url = format!("https://ci-ying.oss-cn-zhangjiakou.aliyuncs.com/v1/ci-yi-list/{word}.txt");
    let response = reqwest::get(&url).await?;
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
) -> String {
    let fetch_req = match prepare_guess(db, group_id).await {
        Ok(req) => req,
        Err(e) => return format!("系统错误：{}", e),
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
        Ok(msg) => msg,
        Err(e) => format!("游戏处理错误：{}", e),
    }
}
