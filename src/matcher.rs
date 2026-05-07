use crate::event::Event;
use simd_json::derived::ValueObjectAccessAsScalar;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

/// 事件匹配器，用于处理交互式等待及 API 响应
///
/// 优化要点：
///   - 使用同步 `Mutex` 替代 `tokio::sync::Mutex`：持锁期间不存在 await 点，
///     避免每事件都在异步运行时上排队。
///   - 通过 `AtomicUsize` 维护等待者数量，dispatch 在无等待者时无锁直通。
///   - 等待者超时后会主动清理自身条目，避免 Vec 因短时高并发等待累积陈旧条目。
pub struct Matcher {
    waiters: Mutex<Vec<Waiter>>,
    waiter_count: AtomicUsize,
}

struct Waiter {
    id: u64,
    // 消息匹配条件
    group_id: Option<i64>,
    user_id: Option<i64>,
    // API 响应匹配条件
    echo: Option<String>,

    sender: oneshot::Sender<Event>,
}

impl Matcher {
    pub fn new() -> Self {
        Self {
            waiters: Mutex::new(Vec::new()),
            waiter_count: AtomicUsize::new(0),
        }
    }

    /// 注册一个消息等待者 (群号/用户)
    pub async fn wait(
        &self,
        group_id: Option<i64>,
        user_id: Option<i64>,
        timeout_duration: Duration,
    ) -> Option<Event> {
        self.wait_internal(group_id, user_id, None, timeout_duration)
            .await
    }

    /// 注册一个响应等待者 (Echo)
    pub async fn wait_resp(&self, echo: String, timeout_duration: Duration) -> Option<Event> {
        self.wait_internal(None, None, Some(echo), timeout_duration)
            .await
    }

    async fn wait_internal(
        &self,
        group_id: Option<i64>,
        user_id: Option<i64>,
        echo: Option<String>,
        timeout_duration: Duration,
    ) -> Option<Event> {
        let (tx, rx) = oneshot::channel();
        // 单调递增 id 用于超时后定位移除
        static WAITER_ID: AtomicUsize = AtomicUsize::new(1);
        let id = WAITER_ID.fetch_add(1, Ordering::Relaxed) as u64;

        {
            let mut guard = self.waiters.lock().unwrap();
            guard.push(Waiter {
                id,
                group_id,
                user_id,
                echo,
                sender: tx,
            });
            self.waiter_count.store(guard.len(), Ordering::Release);
        }

        match tokio::time::timeout(timeout_duration, rx).await {
            Ok(Ok(event)) => Some(event),
            _ => {
                // 超时或被取消：主动清理自身条目，避免内存累积
                let mut guard = self.waiters.lock().unwrap();
                if let Some(idx) = guard.iter().position(|w| w.id == id) {
                    guard.swap_remove(idx);
                    self.waiter_count.store(guard.len(), Ordering::Release);
                }
                None
            }
        }
    }

    /// 尝试分发事件给等待者。如果事件被消费（匹配成功），返回 None；否则返回原事件。
    pub async fn dispatch(&self, event: Event) -> Option<Event> {
        // 快速路径：当前没有等待者，直接放行
        if self.waiter_count.load(Ordering::Acquire) == 0 {
            return Some(event);
        }

        let g_id = event
            .get_i64("group_id")
            .or_else(|| event.get_u64("group_id").map(|v| v as i64));
        let u_id = event
            .get_i64("user_id")
            .or_else(|| event.get_u64("user_id").map(|v| v as i64));
        let echo = event.get_str("echo").map(|s| s.to_string());

        // 如果既不是群消息/私聊消息，也不是 API 响应，直接放行
        if g_id.is_none() && u_id.is_none() && echo.is_none() {
            return Some(event);
        }

        let waiter_opt = {
            let mut guard = self.waiters.lock().unwrap();

            // 寻找匹配者
            let index = guard.iter().position(|w| {
                // 1. 优先匹配 echo (API 响应)
                if let Some(req_echo) = &w.echo {
                    if let Some(resp_echo) = &echo {
                        return req_echo == resp_echo;
                    }
                    return false;
                }

                // 2. 匹配消息 (Group / User)
                // 如果等待者没有 echo 限制，且事件也没有 echo (即普通消息)，则进行 ID 匹配
                if echo.is_none() {
                    let match_group = w.group_id.is_none() || w.group_id == g_id;
                    let match_user = w.user_id.is_none() || w.user_id == u_id;
                    return match_group && match_user;
                }

                false
            });

            if let Some(idx) = index {
                let waiter = guard.swap_remove(idx);
                self.waiter_count.store(guard.len(), Ordering::Release);
                Some(waiter)
            } else {
                None
            }
        };

        if let Some(waiter) = waiter_opt {
            // 发送事件给等待者。忽略错误（如等待者已超时）
            let _ = waiter.sender.send(event);
            None // 事件被消费
        } else {
            Some(event) // 无匹配，返还事件
        }
    }
}
