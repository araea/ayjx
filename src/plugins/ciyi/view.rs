//! 词意游戏的「回应模型」。
//!
//! 引擎只产出结构化结果，怎么呈现由外层决定：默认排版成卡片图（见 `card.rs`），
//! 截图不可用时退回这里的纯文本。两条路径共用同一份数据，图里说的和文本里说的
//! 永远是同一件事。

use chrono::{DateTime, Duration, Utc};

/// 一条提示：某个猜过的词在语义排名里的位置，以及它的左右邻居。
///
/// `prev` 是排名更靠前（离答案更近）那个词的**末字**，`next` 是排名更靠后
/// 那个词的**首字**——这正是词意原本的提示规则：只漏一个字，另一个字留白。
#[derive(Debug, Clone)]
pub struct HintRow {
    pub rank: usize,
    pub prev: String,
    pub word: String,
    pub next: String,
    /// 本次刚猜出来的那一条，卡片里会单独highlight
    pub fresh: bool,
}

impl HintRow {
    /// 还原成纯文本形态：`？佩 ) 东西 ( 冥？ #16`
    pub fn to_text(&self) -> String {
        format!(
            "？{} ) {} ( {}？ #{}",
            self.prev, self.word, self.next, self.rank
        )
    }
}

/// 一局进行中的盘面
#[derive(Debug, Clone)]
pub struct Board {
    /// 额外的一句话提示（重复猜测、未进入排名等）
    pub notice: Option<String>,
    /// 已按名次排好、并截断到配置条数的提示
    pub rows: Vec<HintRow>,
    /// 因截断而没画出来的条数
    pub hidden: usize,
    /// 本局累计猜测次数（含未命中排名的词）
    pub guesses: usize,
    /// 命中排名的词数
    pub hits: usize,
    /// 排名池大小，用于换算「接近度」
    pub pool: usize,
}

/// 猜中时的揭晓
#[derive(Debug, Clone)]
pub struct Win {
    pub answer: String,
    pub winner: String,
    pub guesses: usize,
    pub hits: usize,
    pub started_at: DateTime<Utc>,
}

impl Win {
    /// 本局从开局到猜中的时长，短于一分钟就不必显示
    pub fn elapsed(&self) -> Option<String> {
        let span = Utc::now() - self.started_at;
        if span < Duration::minutes(1) {
            return None;
        }
        let hours = span.num_hours();
        let minutes = span.num_minutes() % 60;
        Some(match hours {
            0 => format!("{minutes} 分钟"),
            _ => format!("{hours} 小时 {minutes} 分"),
        })
    }
}

#[derive(Debug, Clone)]
pub struct RankItem {
    pub name: String,
    pub score: i64,
}

#[derive(Debug, Clone)]
pub struct RankBoard {
    pub title: String,
    pub subtitle: String,
    pub items: Vec<RankItem>,
}

/// 插件对一次交互的完整回应
#[derive(Debug, Clone)]
pub enum Reply {
    Board(Board),
    Win(Win),
    Rank(RankBoard),
    Help,
    Rules,
    /// 一句话反馈（输入有误、系统错误、模式切换等），走纯文本
    Notice(String),
}

impl Reply {
    /// 是否值得为它启动一次无头浏览器。
    ///
    /// 「不在词库中」这类即时纠错要的是快，出图反而慢半拍、还刷屏；
    /// 盘面、揭晓、排行榜、说明这些「要被看、被翻回去看」的内容才配图。
    pub fn wants_card(&self) -> bool {
        !matches!(self, Reply::Notice(_))
    }

    /// 纯文本形态：截图不可用时的兜底，也是配置关掉图片后的常规形态
    pub fn to_text(&self) -> String {
        match self {
            Reply::Board(board) => board.to_text(),
            Reply::Win(win) => format!(
                "恭喜你猜对了！\n答案：{}\n猜测：{} 次",
                win.answer, win.guesses
            ),
            Reply::Rank(rank) => {
                if rank.items.is_empty() {
                    return "当前还没有人猜对过哦！".to_string();
                }
                rank.items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| format!("{}. {} {}", i + 1, item.name, item.score))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Reply::Help => COMMANDS.to_string(),
            Reply::Rules => RULES.to_string(),
            Reply::Notice(text) => text.clone(),
        }
    }
}

impl Board {
    fn to_text(&self) -> String {
        let mut out = String::new();
        if let Some(notice) = &self.notice {
            out.push_str(notice);
            out.push('\n');
        }
        if self.rows.is_empty() {
            out.push_str("还没有命中排名的词，再试一个。");
            return out;
        }
        for (i, row) in self.rows.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, row.to_text()));
        }
        if self.hidden > 0 {
            out.push_str("...");
        } else {
            out.pop();
        }
        out
    }
}

pub const COMMANDS: &str = "\
词意帮助/词意指令 - 查看插件指令列表
词意玩法/词意规则 - 查看词意游戏规则
词意猜测 [词语] - 猜测两字词语
词意榜 - 查看当前频道的词意排行榜
词意全榜 - 查看所有人的词意排行榜
切换猜测模式 - 切换是否可以直接发送词语猜测";

pub const RULES: &str = "\
目标
    猜出系统选择的两字词语

反馈
    每次猜测后，获得：
    - 与目标词语的相似度排名
    - 相邻词提示

示例
    1. ？器 ) 镯子 ( 玉？   #14
    2. ？子 ) 玉佩 ( 东？   #15
    3. ？佩 ) 东西 ( 冥？   #16

    #14   → 相似度排名（越小越近）
    玉？   → 相邻词提示（？为“佩”）

周期
    每日一词，猜对则次日刷新
    系统记录猜对次数，可查排行";

/// 指令列表：卡片版按「指令 / 说明」两栏排，和纯文本同源
pub const COMMAND_ROWS: [(&str, &str, &str); 6] = [
    ("词意帮助", "词意指令", "查看插件指令列表"),
    ("词意玩法", "词意规则", "查看词意游戏规则"),
    ("词意猜测", "[两字词语]", "提交一次猜测"),
    ("词意榜", "", "本群的猜中次数排行"),
    ("词意全榜", "", "所有群的猜中次数排行"),
    ("切换猜测模式", "", "是否允许直接发两字词猜测"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn row(rank: usize, prev: &str, word: &str, next: &str) -> HintRow {
        HintRow {
            rank,
            prev: prev.into(),
            word: word.into(),
            next: next.into(),
            fresh: false,
        }
    }

    #[test]
    fn hint_row_text_matches_classic_format() {
        assert_eq!(row(16, "佩", "东西", "冥").to_text(), "？佩 ) 东西 ( 冥？ #16");
    }

    #[test]
    fn board_text_marks_truncation() {
        let board = Board {
            notice: None,
            rows: vec![row(14, "器", "镯子", "玉"), row(15, "子", "玉佩", "东")],
            hidden: 3,
            guesses: 5,
            hits: 5,
            pool: 1000,
        };
        let text = board.to_text();
        assert!(text.starts_with("1. ？器 ) 镯子 ( 玉？ #14\n"));
        assert!(text.ends_with("..."), "还有未显示的提示时应保留省略号");
    }

    #[test]
    fn board_text_without_hidden_has_no_ellipsis() {
        let board = Board {
            notice: Some("「玉佩」已经猜过了".into()),
            rows: vec![row(15, "子", "玉佩", "东")],
            hidden: 0,
            guesses: 1,
            hits: 1,
            pool: 1000,
        };
        let text = board.to_text();
        assert!(text.starts_with("「玉佩」已经猜过了\n"));
        assert!(!text.ends_with('\n') && !text.ends_with("..."));
    }
}
