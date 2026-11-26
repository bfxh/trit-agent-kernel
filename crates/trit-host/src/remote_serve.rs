//! 远程子 agent 的**参考服务端**：HTTP/1.1 单帧 POST。
//!
//! 除只读 `GET /health` 外，只实现一条最小、可审计的线：`POST <path>`，请求体是一帧，
//! 回一帧，`Connection: close`。不是通用 HTTP 服务器，也不做长连接 / 分块 / 重定向。
//!
//! 不变量：
//!
//! 1. **有界读**：`Content-Length` 缺失、超过 [`MAX_FRAME_BYTES`] + 4、或实际
//!    字节数不足，一律拒绝，绝不无限读；
//! 2. **鉴权先于执行**：配置了 token 时，缺 `Authorization: Bearer <token>`
//!    直接 401，帧不解码；期望头在启动时转成 SHA-256 摘要，实际头也摘要后
//!    用固定长度常量时间比较，不逐字节泄漏 token 前缀；
//! 3. **只回合法帧**：响应体永远是 `trit_orch::encode_frame` 的产物；
//!    失败模式回 `Failed` 帧，而不是让宿主把错误当成完成；
//! 4. **一连接一请求**：读满一帧即回包并关闭，便于宿主侧超时语义保持简单；
//!    请求行与头部也有界（单行 8 KiB、总计 64 KiB、最多 64 个），超限直接 431，
//!    不把整行读进内存；
//! 5. **带键才去重**：`X-Trit-Idempotency` 必须等于帧里的 `task@epoch`，否则 400；
//!    命中缓存直接回首次响应、不再执行（`sleep_ms` 也不生效）；缓存同时限制
//!    条数 [`MAX_CACHED_RESPONSES`]、逻辑字节 `max_cache_bytes`（默认 64 MiB）与
//!    保留时间 `cache_ttl`（默认 1h，0 关闭），按写入顺序 FIFO 淘汰，单条超预算
//!    时不会缓存；过期条目在命中或写入时清理，不再回放。没有共享账本时，同键并发请求
//!    先在端点内排队，只有第一个执行，其余等待并回放首次结果；等待受 `exec_timeout`
//!    约束（未配置时最多等 30s），首个结果未缓存（如 Aborted）时下一个继续执行。
//!    没有头就按普通请求处理；
//! 6. **账本优先**：配置 `ledger` 后，带键请求先向共享账本 claim；账本返回完成帧
//!    就直接回放，返回占位则轮询等待，拿到执行权才执行，执行后必须 complete。
//!    账本不可达时回 503，宁可不执行也不冒重复执行的风险。`Silent` 模式不产生
//!    响应帧，无法登记完成，账本占位会留到 TTL 过期；
//! 7. **Exec 模式真实执行**：spawn `subagent-run`（或等价命令），请求帧走 stdin、
//!    响应帧走 stdout；只有子进程判定完成才回 `Completed`，进程错误/超时按
//!    `Failed`/`Aborted` 回帧，不误报完成。配 `exec_workspace_root` 时按任务 ID
//!    建 `<root>[/<scope>]/<task>` 并展开 args 里的 `{workspace}` / `{task}`；任务
//!    ID 含路径分隔符或为 `.`/`..` 时直接回 `Failed`，不落盘；
//! 8. **取消传播**：`POST /cancel`（origin 上的固定路径）+ `X-Trit-Idempotency`
//!    按幂等键置位所有在途请求（同键并发时每个持有者都会被取消）；等待账本、
//!    `sleep_ms` 与执行前都会检查，Exec 用可取消进程跑法直接 kill 子进程。取消回合法 `Aborted` 帧，并照常登记
//!    账本/本地缓存，避免后续重试再次执行同一键；
//! 9. **连接有界**：`max_connections` 默认 64；超额连接直接 503，不排队、不 spawn，
//!    防止慢客户端把服务端线程与内存堆爆；
//! 10. **在途租约续约与持有者令牌**：拿到账本占位后按 `ledger_renew_interval`
//!     （默认 10s）后台调用 `/renew`，执行超过 `pending_ttl` 也不会被其他节点
//!     误判过期；`/renew` 与 `/complete` 都携带 claim 返回的 `X-Trit-Lease`，
//!     过期接管后旧持有者不能再续租或写完成。心跳收到 `403`/`404` 或已完成标记
//!     会立即置位取消令牌：睡眠与 Exec 子进程都被中止，服务端回 `Aborted`（带
//!     `X-Trit-Lease-Lost: 1`）且不登记完成，避免旧持有者继续做无效副作用。若
//!     执行完才发现完成帧已由别人写入（`/complete` 403），同样回标记过的
//!     `Aborted`，绝不把本机旧结果当成获胜帧返回。网络分区导致心跳请求完全无法
//!     送达时，旧执行者仍可能在本地跑完，这部分需任务幂等兜底。
//! 11. **执行阶段墙钟有界**：配置 `exec_timeout` 后，deadline 从请求体解码完成
//!     后启动，覆盖账本等待、`sleep_ms` 与 Exec 子进程；头/体上传由请求读取总时限、
//!     30s socket 空闲超时和长度上限共同约束，不计入该 deadline。到点先标记 `timed_out` 再取消同一令牌，最终强制 `Aborted`、
//!     跳过 complete 与本地缓存，并带 `X-Trit-Deadline-Exceeded: 1` 让调度层
//!     区分“节点失联”与“节点健康但请求超预算”。
//! 12. **健康探测只读**：`GET /health` 沿用同一 Bearer 鉴权，鉴权通过即回 200 空体；
//!     不解析任务帧、不读写幂等缓存/账本、不执行任务，供 `remote-plan --probe`
//!     在重新放行隔离节点前做一次轻量可达性检查。
//! 13. **请求读取总时限**：请求行 + 头部 + 请求体共享 `request_read_timeout`
//!     （默认 60s；`--request-read-timeout-ms`，0 关闭）。慢速滴流即使每次
//!     socket 读取都没超时，也会在总截止点回 `408`，不进入派发/执行阶段。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use trit_orch::{
    CancelToken, DelegationStatus, MAX_FRAME_BYTES, RemoteEndpoint, SubagentRequest,
    SubagentResponse, decode_frame, encode_frame,
};

use crate::auth::{AUTH_DIGEST_LEN, authorization_matches, bearer_digest};
use crate::connection_limit::{ConnectionSlots, DEFAULT_MAX_CONNECTIONS};
use crate::http_read::{
    DEFAULT_REQUEST_READ_TIMEOUT, DeadlineStream, HeaderReadError, IO_TIMEOUT, MAX_HEADER_BYTES,
    MAX_HEADER_LINE_BYTES, MAX_HEADERS, is_timeout, read_http_line,
};
use crate::remote_ledger::{self, Claim};
use crate::remote_subagent::{
    CANCEL_PATH, DEADLINE_EXCEEDED_HEADER, HEALTH_PATH, LEASE_LOST_HEADER, idempotency_scope,
};
use crate::subagent_process::{SubagentProcessConfig, run_subagent_reply_cancellable};

/// 等待容量/执行期间检查取消的步长。
const CANCEL_CHECK_STEP: Duration = Duration::from_millis(25);

/// 预取消但尚未到达的键上限，防止 `/cancel` 被用来无限占用内存。
const MAX_PRE_CANCELLED: usize = 1024;

/// 账本占位租约的默认续约间隔；应小于账本 `pending_ttl`。
const DEFAULT_LEDGER_RENEW_INTERVAL: Duration = Duration::from_secs(10);

/// 幂等缓存条数上限：按写入顺序 FIFO 淘汰，不做 LRU。
const MAX_CACHED_RESPONSES: usize = 1024;

/// 同键在途闸门上限：每个键一个闸门，活跃请求之外的条目会被清理。
const MAX_INFLIGHT_GATES: usize = 1024;

/// 没有配置执行时限时，同键等待的兜底上限，避免半开客户端无限占住闸门。
const KEY_GATE_WAIT: Duration = Duration::from_secs(30);

/// 同键闸门的轮询间隔：等待期间按期检查取消与执行时限。
const KEY_GATE_POLL: Duration = Duration::from_millis(25);

/// 幂等缓存逻辑字节上限默认值：单条帧最大 8 MiB，只限条数会放大到 GiB 级内存。
const DEFAULT_MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// 本地幂等缓存的默认保留时间（1h）；`with_cache_ttl(Duration::ZERO)` 关闭按时间淘汰。
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// 幂等缓存：`"<task>@<epoch>" -> 首次响应帧`，同时限制条数、逻辑字节与保留时间。
type ResponseCache = Arc<Mutex<ResponseCacheState>>;

#[derive(Default)]
struct ResponseCacheState {
    entries: BTreeMap<String, CachedResponse>,
    bytes: usize,
    next_seq: u64,
}

struct CachedResponse {
    frame: Vec<u8>,
    /// 逻辑占用 = key 字节 + 帧字节。
    bytes: usize,
    /// 写入顺序，用于 FIFO 淘汰（BTreeMap 的 key 顺序不是写入顺序）。
    seq: u64,
    /// 写入时刻；超过缓存 TTL 后不再回放。
    inserted_at: Instant,
}

impl ResponseCacheState {
    fn get(&mut self, key: &str, ttl: Option<Duration>) -> Option<&CachedResponse> {
        self.prune_expired(Instant::now(), ttl);
        self.entries.get(key)
    }

    fn insert(&mut self, key: String, frame: Vec<u8>, max_bytes: usize, ttl: Option<Duration>) {
        let now = Instant::now();
        self.prune_expired(now, ttl);
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old.bytes);
        }
        let bytes = key.len().saturating_add(frame.len());
        self.next_seq = self.next_seq.wrapping_add(1);
        let seq = self.next_seq;
        self.entries.insert(
            key,
            CachedResponse {
                frame,
                bytes,
                seq,
                inserted_at: now,
            },
        );
        self.bytes = self.bytes.saturating_add(bytes);
        while self.entries.len() > MAX_CACHED_RESPONSES {
            self.evict_oldest();
        }
        while self.bytes > max_bytes && !self.entries.is_empty() {
            self.evict_oldest();
        }
    }

    /// 删除超过 TTL 的条目；TTL 越小，保留窗口越短。
    fn prune_expired(&mut self, now: Instant, ttl: Option<Duration>) {
        let Some(ttl) = ttl else {
            return;
        };
        let before = self.entries.len();
        self.entries
            .retain(|_, cached| now.saturating_duration_since(cached.inserted_at) < ttl);
        if self.entries.len() != before {
            self.bytes = self.entries.values().map(|cached| cached.bytes).sum();
        }
    }

    fn evict_oldest(&mut self) {
        let Some(key) = self
            .entries
            .iter()
            .min_by_key(|(_, cached)| cached.seq)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old.bytes);
        }
    }
}

/// 同键在途请求的端点内闸门：无共享账本时，只有一个请求执行，其余等待并回放缓存。
#[derive(Default)]
struct KeyGates {
    inner: Mutex<GateTable>,
}

#[derive(Default)]
struct GateTable {
    gates: BTreeMap<String, Weak<KeyGate>>,
}

struct KeyGate {
    held: Mutex<bool>,
    available: Condvar,
}

impl Default for KeyGate {
    fn default() -> Self {
        Self {
            held: Mutex::new(false),
            available: Condvar::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateError {
    Cancelled,
    Deadline,
    Capacity,
}

impl KeyGates {
    /// 取得某个键的执行权；等待期间检查取消与截止时间。
    ///
    /// 闸门条目用 `Weak` 保存，最后一个持有者退出后会被下一次插入顺手清理；
    /// 活跃键超过 [`MAX_INFLIGHT_GATES`] 时直接返回容量错误，不再无限建闸门。
    fn acquire(
        &self,
        key: &str,
        cancel: &CancelToken,
        deadline: Instant,
    ) -> Result<KeyGateGuard, GateError> {
        let gate = {
            let mut table = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(gate) = table.gates.get(key).and_then(Weak::upgrade) {
                gate
            } else {
                table.gates.retain(|_, gate| gate.strong_count() > 0);
                if table.gates.len() >= MAX_INFLIGHT_GATES {
                    return Err(GateError::Capacity);
                }
                let gate = Arc::new(KeyGate::default());
                table.gates.insert(key.to_string(), Arc::downgrade(&gate));
                gate
            }
        };

        let mut held = gate
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if cancel.is_cancelled() {
                return Err(GateError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(GateError::Deadline);
            }
            if !*held {
                *held = true;
                drop(held);
                return Ok(KeyGateGuard { gate });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait = remaining.min(KEY_GATE_POLL);
            if wait.is_zero() {
                continue;
            }
            let (next, _) = gate
                .available
                .wait_timeout(held, wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            held = next;
        }
    }
}

struct KeyGateGuard {
    gate: Arc<KeyGate>,
}

impl Drop for KeyGateGuard {
    fn drop(&mut self) {
        let mut held = self
            .gate
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *held = false;
        self.gate.available.notify_all();
    }
}

/// 在途请求的取消登记：按幂等键；同一个键可能有多个并发请求，全部保留。
#[derive(Default)]
struct ActiveCancels {
    inner: Mutex<CancelRegistry>,
}

#[derive(Default)]
struct CancelRegistry {
    active: BTreeMap<String, Vec<(u64, CancelToken)>>,
    /// 每个 `CancelScope` 的唯一编号，避免先结束的请求删掉后来者的登记。
    next_id: u64,
    /// 取消先于请求到达的键；请求登记时立即继承取消状态。
    pre: BTreeSet<String>,
}

impl ActiveCancels {
    /// 登记一个请求并返回自动注销的守卫。
    fn scope(&self, key: &str) -> CancelScope<'_> {
        let token = CancelToken::new();
        let id = {
            let mut registry = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if registry.pre.remove(key) {
                token.cancel();
            }
            registry.next_id = registry.next_id.wrapping_add(1);
            let id = registry.next_id;
            registry
                .active
                .entry(key.to_string())
                .or_default()
                .push((id, token.clone()));
            id
        };
        CancelScope {
            registry: self,
            key: key.to_string(),
            id,
            token,
        }
    }

    /// 取消一个键：该键当前所有在途请求都置位；尚未到达的键先暂存。
    fn cancel(&self, key: &str) {
        let mut registry = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(tokens) = registry.active.get(key) {
            for (_, token) in tokens {
                token.cancel();
            }
            return;
        }
        if registry.pre.len() >= MAX_PRE_CANCELLED
            && let Some(oldest) = registry.pre.iter().next().cloned()
        {
            registry.pre.remove(&oldest);
        }
        registry.pre.insert(key.to_string());
    }
}

struct CancelScope<'a> {
    registry: &'a ActiveCancels,
    key: String,
    id: u64,
    token: CancelToken,
}

impl CancelScope<'_> {
    fn token(&self) -> &CancelToken {
        &self.token
    }
}

impl Drop for CancelScope<'_> {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(tokens) = registry.active.get_mut(&self.key) else {
            return;
        };
        tokens.retain(|(id, _)| *id != self.id);
        if tokens.is_empty() {
            registry.active.remove(&self.key);
        }
    }
}

/// 账本租约心跳：Drop 时关闭通道，后台线程退出；不 join，避免卡住完成响应。
struct LedgerHeartbeat {
    stop: Option<std::sync::mpsc::Sender<()>>,
}

impl Drop for LedgerHeartbeat {
    fn drop(&mut self) {
        drop(self.stop.take());
    }
}

fn start_ledger_heartbeat(
    endpoint: RemoteEndpoint,
    key: String,
    lease: String,
    interval: Duration,
    cancel: CancelToken,
    lease_lost: Arc<AtomicBool>,
) -> Option<LedgerHeartbeat> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("trit-ledger-heartbeat".into())
        .spawn(move || {
            loop {
                match receiver.recv_timeout(interval) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        match remote_ledger::renew_checked(&endpoint, &key, &lease) {
                            Ok(()) => {}
                            Err(
                                remote_ledger::RenewError::LeaseLost
                                | remote_ledger::RenewError::UnknownKey
                                | remote_ledger::RenewError::Completed,
                            ) => {
                                // 租约被接管、账本遗忘该键或任务已完成：停止本地执行，避免重复副作用。
                                lease_lost.store(true, Ordering::SeqCst);
                                cancel.cancel();
                                break;
                            }
                            Err(_) => {}
                        }
                    }
                }
            }
        })
        .ok()?;
    Some(LedgerHeartbeat { stop: Some(sender) })
}

/// 执行时限守卫：Drop 时关闭通道，后台计时线程退出；不 join。
struct ExecutionDeadline {
    stop: Option<std::sync::mpsc::Sender<()>>,
}

impl Drop for ExecutionDeadline {
    fn drop(&mut self) {
        drop(self.stop.take());
    }
}

fn start_execution_deadline(
    timeout: Duration,
    cancel: CancelToken,
    timed_out: Arc<AtomicBool>,
) -> Option<ExecutionDeadline> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("trit-exec-deadline".into())
        .spawn(move || {
            match receiver.recv_timeout(timeout) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // 先记录原因再取消：最终响应要走 Aborted 且不登记完成。
                    timed_out.store(true, Ordering::SeqCst);
                    cancel.cancel();
                }
            }
        })
        .ok()?;
    Some(ExecutionDeadline { stop: Some(sender) })
}

/// 服务端应答模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMode {
    /// 回 `Completed`（默认）。
    Echo,
    /// 回 `Failed`：让宿主走“有响应但没完成”的分支。
    Fail,
    /// 回 200 空体：让宿主走“没有合法响应”的分支。
    Silent,
    /// 真实执行：按配置 spawn `subagent-run`（或等价命令）并回它的响应帧。
    Exec,
}

/// 参考服务端配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteServerConfig {
    /// 监听地址；`127.0.0.1:0` 表示由系统分配端口。
    pub bind: String,
    /// 可选 bearer token；配置后所有请求都要带同一个值。
    pub token: Option<String>,
    /// 应答模式。
    pub mode: RemoteMode,
    /// 回包前的人为延迟（毫秒），用于验证宿主超时。
    pub sleep_ms: u64,
    /// 可选的共享幂等账本；配置后带键请求以账本为准，本地缓存不再参与。
    pub ledger: Option<RemoteEndpoint>,
    /// 账本占位租约的续约间隔；执行时间可能超过 `pending_ttl` 时必须小于它。
    pub ledger_renew_interval: Duration,
    /// `Exec` 模式下的子 agent 进程配置；其他模式必须为空。
    pub exec: Option<SubagentProcessConfig>,
    /// `Exec` 模式下按任务隔离的 workspace 根：请求的任务 ID 作为子目录，
    /// args 里的 `{workspace}` 会在执行前替换成该目录。
    pub exec_workspace_root: Option<PathBuf>,
    /// 同时在处理的连接上限；超限直接回 503，不再 spawn 线程。
    pub max_connections: usize,
    /// 本地幂等缓存的逻辑字节上限（key + 响应帧）；超限按写入顺序 FIFO 淘汰。
    /// 只对未接账本的带键请求生效；单条超过上限时不会被缓存。
    pub max_cache_bytes: usize,
    /// 本地幂等缓存的保留时间；`None` 表示不按时间淘汰，只受条数/字节限制。
    /// 只对未接账本的带键请求生效。
    pub cache_ttl: Option<Duration>,
    /// 单次请求的墙钟执行上限；含账本等待、`sleep_ms` 与 `Exec` 子进程。
    /// `None` 不限制；超过后取消在途执行并回 `Aborted`，且不登记完成。
    pub exec_timeout: Option<Duration>,
    /// 请求行 + 头部 + 请求体的总读取时限；`None` 不限制。
    /// 到点回 `408`，不进入执行阶段；执行阶段另受 `exec_timeout` 约束。
    pub request_read_timeout: Option<Duration>,
}

impl RemoteServerConfig {
    pub fn new(bind: impl Into<String>) -> Self {
        Self {
            bind: bind.into(),
            token: None,
            mode: RemoteMode::Echo,
            sleep_ms: 0,
            ledger: None,
            ledger_renew_interval: DEFAULT_LEDGER_RENEW_INTERVAL,
            exec: None,
            exec_workspace_root: None,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_cache_bytes: DEFAULT_MAX_CACHE_BYTES,
            cache_ttl: Some(DEFAULT_CACHE_TTL),
            exec_timeout: None,
            request_read_timeout: Some(DEFAULT_REQUEST_READ_TIMEOUT),
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn with_mode(mut self, mode: RemoteMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_sleep_ms(mut self, sleep_ms: u64) -> Self {
        self.sleep_ms = sleep_ms;
        self
    }

    pub fn with_ledger(mut self, ledger: RemoteEndpoint) -> Self {
        self.ledger = Some(ledger);
        self
    }

    pub fn with_ledger_renew_interval(mut self, interval: Duration) -> Self {
        self.ledger_renew_interval = interval;
        self
    }

    pub fn with_exec(mut self, exec: SubagentProcessConfig) -> Self {
        self.mode = RemoteMode::Exec;
        self.exec = Some(exec);
        self
    }

    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections.max(1);
        self
    }

    pub fn with_max_cache_bytes(mut self, max_cache_bytes: usize) -> Self {
        self.max_cache_bytes = max_cache_bytes.max(1);
        self
    }

    /// 设置本地幂等缓存的保留时间；`Duration::ZERO` 表示不按时间淘汰。
    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = (!ttl.is_zero()).then_some(ttl);
        self
    }

    /// 设置单次请求的墙钟上限；`Duration::ZERO` 表示不限制。
    pub fn with_exec_timeout(mut self, timeout: Duration) -> Self {
        self.exec_timeout = (!timeout.is_zero()).then_some(timeout);
        self
    }

    /// 设置请求行 + 头部 + 请求体的总读取时限；`Duration::ZERO` 表示不限制。
    pub fn with_request_read_timeout(mut self, timeout: Duration) -> Self {
        self.request_read_timeout = (!timeout.is_zero()).then_some(timeout);
        self
    }

    /// 按任务隔离 workspace：`<root>/<sanitized task id>`。
    pub fn with_exec_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.exec_workspace_root = Some(root.into());
        self
    }
}

/// 已绑定的参考服务端。
pub struct RemoteServer {
    listener: TcpListener,
    config: RemoteServerConfig,
    auth: Option<[u8; AUTH_DIGEST_LEN]>,
    cache: ResponseCache,
    cancels: Arc<ActiveCancels>,
    gates: Arc<KeyGates>,
    executions: Arc<AtomicU64>,
    slots: Arc<ConnectionSlots>,
}

impl RemoteServer {
    /// 绑定监听地址；失败即返回，不重试。
    pub fn bind(config: RemoteServerConfig) -> std::io::Result<Self> {
        if let Some(ledger) = config.ledger.as_ref() {
            ledger.validate().map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
            })?;
        }
        if config.ledger.is_some() && config.ledger_renew_interval.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ledger renew interval must be positive",
            ));
        }
        match (config.mode, config.exec.is_some()) {
            (RemoteMode::Exec, false) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "exec mode requires exec process config",
                ));
            }
            (mode, true) if mode != RemoteMode::Exec => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "exec process config is only valid in exec mode",
                ));
            }
            _ => {}
        }
        if config.exec_workspace_root.is_some() && config.mode != RemoteMode::Exec {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "exec workspace root is only valid in exec mode",
            ));
        }
        if let Some(exec) = config.exec.as_ref()
            && exec.args.iter().any(|arg| arg.contains("{workspace}"))
            && config.exec_workspace_root.is_none()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "exec args use {workspace} but exec workspace root is not configured",
            ));
        }
        let auth = config.token.as_deref().map(bearer_digest);
        let listener = TcpListener::bind(config.bind.as_str())?;
        let slots = ConnectionSlots::new(config.max_connections);
        Ok(Self {
            listener,
            config,
            auth,
            cache: Arc::default(),
            cancels: Arc::default(),
            gates: Arc::default(),
            executions: Arc::default(),
            slots,
        })
    }

    /// 实际监听地址（`--bind 127.0.0.1:0` 时用它对外公布）。
    pub fn addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 未命中缓存、真正进入执行阶段的请求数；用于观测与并发语义测试。
    pub fn executed_requests(&self) -> u64 {
        self.executions.load(Ordering::SeqCst)
    }

    /// 服务一个连接后返回；测试与单次演示用。
    pub fn serve_once(&self) -> std::io::Result<()> {
        let (mut stream, _) = self.listener.accept()?;
        let Some(_slot) = self.slots.try_acquire() else {
            return respond(&mut stream, 503, "Service Unavailable", b"");
        };
        handle(
            stream,
            &self.config,
            self.auth.as_ref(),
            &self.cache,
            &self.cancels,
            &self.gates,
            &self.executions,
        )
    }

    /// 接受并并发处理固定数量的连接后返回；用于测试与有界演练。
    pub fn serve_connections(&self, count: usize) -> std::io::Result<()> {
        let mut workers = Vec::with_capacity(count);
        for _ in 0..count {
            let (mut stream, _) = self.listener.accept()?;
            let Some(slot) = self.slots.try_acquire() else {
                let _ = respond(&mut stream, 503, "Service Unavailable", b"");
                continue;
            };
            let config = self.config.clone();
            let auth = self.auth;
            let cache = Arc::clone(&self.cache);
            let cancels = Arc::clone(&self.cancels);
            let gates = Arc::clone(&self.gates);
            let executions = Arc::clone(&self.executions);
            workers.push(std::thread::spawn(move || {
                let _slot = slot;
                let _ = handle(
                    stream,
                    &config,
                    auth.as_ref(),
                    &cache,
                    &cancels,
                    &gates,
                    &executions,
                );
            }));
        }
        for worker in workers {
            worker
                .join()
                .map_err(|_| std::io::Error::other("server worker panicked"))?;
        }
        Ok(())
    }

    /// 持续服务，直到进程被终止；单连接错误只丢弃该连接。
    ///
    /// 每个连接一个线程（参考实现不做线程池）：跨机 fleet 会从多个 worker
    /// 同时打到一个端点，串行处理会让 `max_in_flight > 1` 的容量形同虚设。
    pub fn serve_forever(&self) -> std::io::Result<()> {
        loop {
            let (mut stream, _) = self.listener.accept()?;
            let Some(slot) = self.slots.try_acquire() else {
                // 超额连接不排队、不 spawn：直接 503，保护服务端线程与内存。
                let _ = respond(&mut stream, 503, "Service Unavailable", b"");
                continue;
            };
            let config = self.config.clone();
            let auth = self.auth;
            let cache = Arc::clone(&self.cache);
            let cancels = Arc::clone(&self.cancels);
            let gates = Arc::clone(&self.gates);
            let executions = Arc::clone(&self.executions);
            std::thread::spawn(move || {
                let _slot = slot;
                let _ = handle(
                    stream,
                    &config,
                    auth.as_ref(),
                    &cache,
                    &cancels,
                    &gates,
                    &executions,
                );
            });
        }
    }
}

/// 处理一个连接：解析请求头 → 鉴权 → 读一帧 → 幂等查缓存/执行 → 回一帧 → 关闭。
fn handle(
    mut stream: TcpStream,
    config: &RemoteServerConfig,
    auth: Option<&[u8; AUTH_DIGEST_LEN]>,
    cache: &ResponseCache,
    cancels: &ActiveCancels,
    gates: &KeyGates,
    executions: &AtomicU64,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(DeadlineStream::new(
        stream.try_clone()?,
        config.request_read_timeout,
    )?);

    let request_line = match read_http_line(&mut reader, MAX_HEADER_LINE_BYTES) {
        Ok(Some(line)) => line,
        Ok(None) | Err(HeaderReadError::Invalid) => {
            return respond(&mut stream, 400, "Bad Request", b"");
        }
        Err(HeaderReadError::TooLarge) => {
            return respond(&mut stream, 431, "Request Header Fields Too Large", b"");
        }
        Err(HeaderReadError::Io(error)) if is_timeout(&error) => {
            return respond(&mut stream, 408, "Request Timeout", b"");
        }
        Err(HeaderReadError::Io(error)) => return Err(error),
    };
    let mut header_bytes = request_line.len();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    if method.is_empty() || path.is_empty() {
        return respond(&mut stream, 400, "Bad Request", b"");
    }
    let is_health = method == "GET" && path == HEALTH_PATH;
    if method != "POST" && !is_health {
        return respond(&mut stream, 405, "Method Not Allowed", b"");
    }

    let mut content_length: Option<usize> = None;
    let mut authorization: Option<String> = None;
    let mut idempotency: Option<String> = None;
    let mut header_count = 0;
    loop {
        let line = match read_http_line(&mut reader, MAX_HEADER_LINE_BYTES) {
            Ok(Some(line)) => line,
            Ok(None) | Err(HeaderReadError::Invalid) => {
                return respond(&mut stream, 400, "Bad Request", b"");
            }
            Err(HeaderReadError::TooLarge) => {
                return respond(&mut stream, 431, "Request Header Fields Too Large", b"");
            }
            Err(HeaderReadError::Io(error)) if is_timeout(&error) => {
                return respond(&mut stream, 408, "Request Timeout", b"");
            }
            Err(HeaderReadError::Io(error)) => return Err(error),
        };
        header_count += 1;
        header_bytes = header_bytes.saturating_add(line.len());
        if header_count > MAX_HEADERS || header_bytes > MAX_HEADER_BYTES {
            return respond(&mut stream, 431, "Request Header Fields Too Large", b"");
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            return respond(&mut stream, 400, "Bad Request", b"");
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => match value.parse::<usize>() {
                Ok(length) => content_length = Some(length),
                Err(_) => return respond(&mut stream, 400, "Bad Request", b""),
            },
            "authorization" => authorization = Some(value.to_string()),
            "x-trit-idempotency" => idempotency = Some(value.to_string()),
            _ => {}
        }
    }

    if let Some(expected) = auth {
        // 启动时缓存期望头的 SHA-256 摘要；比较固定长度摘要，避免逐字节提前返回。
        if !authorization_matches(authorization.as_deref(), expected) {
            return respond(&mut stream, 401, "Unauthorized", b"");
        }
    }

    if is_health {
        // 只报存活与鉴权通过；不派发任务、不读取/登记幂等状态。
        return respond(&mut stream, 200, "OK", b"");
    }

    let Some(length) = content_length else {
        return respond(&mut stream, 411, "Length Required", b"");
    };
    if length > MAX_FRAME_BYTES + 4 {
        return respond(&mut stream, 413, "Payload Too Large", b"");
    }

    if path == CANCEL_PATH {
        if length != 0 {
            return respond(&mut stream, 400, "Bad Request", b"");
        }
        let Some(key) = idempotency.as_deref() else {
            return respond(&mut stream, 400, "Bad Request", b"");
        };
        // 未知键与重复取消都是 no-op；鉴权已在上面完成。
        cancels.cancel(key);
        return respond(&mut stream, 204, "No Content", b"");
    }

    let mut body = vec![0u8; length];
    if let Err(error) = reader.read_exact(&mut body) {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            return respond(&mut stream, 400, "Bad Request", b"");
        }
        if is_timeout(&error) {
            return respond(&mut stream, 408, "Request Timeout", b"");
        }
        return Err(error);
    }

    let request: SubagentRequest = match decode_frame(&mut body) {
        Ok(Some(request)) => request,
        Ok(None) | Err(_) => return respond(&mut stream, 400, "Bad Request", b""),
    };

    // 幂等键必须与帧里的 task@epoch 一致；可选 `<scope>|` 前缀用于隔离不同计划。
    let frame_key = format!("{}@{}", request.task, request.epoch);
    let (key, scope) = match idempotency.as_deref() {
        None => (frame_key, None),
        Some(value) => match idempotency_scope(value, &request.task, request.epoch) {
            Ok(scope) => (value.to_string(), scope.map(str::to_string)),
            Err(_) => return respond(&mut stream, 400, "Bad Request", b""),
        },
    };
    let dedupe = idempotency.is_some();
    let _cancel_scope = dedupe.then(|| cancels.scope(&key));
    // 即使没有幂等键也保留一个本地取消令牌，执行时限与 Exec 子进程共用它。
    let token = _cancel_scope
        .as_ref()
        .map(|scope| scope.token().clone())
        .unwrap_or_default();
    let timed_out = Arc::new(AtomicBool::new(false));
    let request_deadline = config.exec_timeout.map(|timeout| Instant::now() + timeout);
    let _deadline = request_deadline.and_then(|deadline| {
        start_execution_deadline(
            deadline.saturating_duration_since(Instant::now()),
            token.clone(),
            Arc::clone(&timed_out),
        )
    });
    // 无共享账本时，同键请求先在端点内排队：只让第一个执行，其余等待后回放缓存。
    // 等待受执行时限约束；没配时限时用 KEY_GATE_WAIT 兜底，避免无限挂住连接。
    let gate_deadline = request_deadline.unwrap_or_else(|| Instant::now() + KEY_GATE_WAIT);
    let _gate_guard = if dedupe && config.ledger.is_none() {
        match gates.acquire(&key, &token, gate_deadline) {
            Ok(guard) => Some(guard),
            Err(GateError::Cancelled) => {
                return if timed_out.load(Ordering::SeqCst) {
                    respond_aborted_deadline(&mut stream, &request)
                } else {
                    respond_aborted(&mut stream, &request)
                };
            }
            Err(GateError::Deadline) => {
                return if request_deadline.is_some() {
                    respond_aborted_deadline(&mut stream, &request)
                } else {
                    respond(&mut stream, 503, "Service Unavailable", b"")
                };
            }
            Err(GateError::Capacity) => {
                return respond(&mut stream, 503, "Service Unavailable", b"");
            }
        }
    } else {
        None
    };
    let mut ledger_owned = false;
    let mut ledger_lease: Option<String> = None;
    if dedupe {
        if let Some(endpoint) = config.ledger.as_ref() {
            // 账本是跨机权威：先 claim，再执行；等待期内轮询完成响应与取消。
            let deadline = std::time::Instant::now() + Duration::from_secs(endpoint.timeout_secs);
            loop {
                if token.is_cancelled() {
                    // 等待账本期间也可能是 deadline 触发：保留超预算标记，
                    // 否则调度层会把健康节点误判成故障。
                    return if timed_out.load(Ordering::SeqCst) {
                        respond_aborted_deadline(&mut stream, &request)
                    } else {
                        respond_aborted(&mut stream, &request)
                    };
                }
                match remote_ledger::claim(endpoint, &key) {
                    Ok(Claim::New { lease }) => {
                        ledger_owned = true;
                        ledger_lease = Some(lease);
                        break;
                    }
                    Ok(Claim::Done(frame)) => return respond(&mut stream, 200, "OK", &frame),
                    Ok(Claim::Pending) => {
                        if std::time::Instant::now() >= deadline {
                            return respond(&mut stream, 409, "Conflict", b"");
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => return respond(&mut stream, 503, "Service Unavailable", b""),
                }
            }
        } else if let Some(cached) = cache.lock().ok().and_then(|mut guard| {
            guard
                .get(&key, config.cache_ttl)
                .map(|cached| cached.frame.clone())
        }) {
            drop(_gate_guard);
            return respond(&mut stream, 200, "OK", &cached);
        }
    }

    let lease_lost = Arc::new(AtomicBool::new(false));
    let _heartbeat = if ledger_owned {
        match (config.ledger.as_ref(), ledger_lease.as_deref()) {
            (Some(endpoint), Some(lease)) => start_ledger_heartbeat(
                endpoint.clone(),
                key.clone(),
                lease.to_string(),
                config.ledger_renew_interval,
                token.clone(),
                Arc::clone(&lease_lost),
            ),
            _ => None,
        }
    } else {
        None
    };

    executions.fetch_add(1, Ordering::SeqCst);
    let cancelled = if config.sleep_ms > 0 {
        sleep_unless_cancelled(config.sleep_ms, Some(&token))
    } else {
        token.is_cancelled()
    };

    let (status, content, tokens) = if cancelled {
        (DelegationStatus::Aborted, Vec::new(), 0)
    } else {
        match config.mode {
            RemoteMode::Echo => (
                DelegationStatus::Completed,
                format!("remote echo {}@{}", request.task, request.epoch).into_bytes(),
                0,
            ),
            RemoteMode::Fail => (
                DelegationStatus::Failed {
                    message: "remote echo configured to fail".into(),
                },
                format!("remote echo {}@{}", request.task, request.epoch).into_bytes(),
                0,
            ),
            RemoteMode::Silent => {
                if lease_lost.load(Ordering::SeqCst) || timed_out.load(Ordering::SeqCst) {
                    (DelegationStatus::Aborted, Vec::new(), 0)
                } else {
                    return respond(&mut stream, 200, "OK", b"");
                }
            }
            RemoteMode::Exec => {
                let Some(exec) = config.exec.as_ref() else {
                    return respond(&mut stream, 500, "Internal Server Error", b"");
                };
                match exec_for_request(
                    exec,
                    config.exec_workspace_root.as_deref(),
                    &request.task,
                    scope.as_deref(),
                ) {
                    Ok(exec) => {
                        let reply = run_subagent_reply_cancellable(&exec, &request, &token);
                        match reply {
                            Ok(reply) => (reply.result.status, reply.content, reply.result.tokens),
                            Err(error) => (
                                DelegationStatus::Failed {
                                    message: error.to_string(),
                                },
                                Vec::new(),
                                0,
                            ),
                        }
                    }
                    Err(message) => (DelegationStatus::Failed { message }, Vec::new(), 0),
                }
            }
        }
    };
    let lease_lost_now = lease_lost.load(Ordering::SeqCst);
    let timed_out_now = timed_out.load(Ordering::SeqCst);
    let aborted_now = lease_lost_now || timed_out_now;
    let (status, content, tokens) = if aborted_now {
        (DelegationStatus::Aborted, Vec::new(), 0)
    } else {
        (status, content, tokens)
    };
    let response = SubagentResponse {
        task: request.task.clone(),
        epoch: request.epoch,
        status,
        content,
        tokens,
    };
    let frame = match encode_frame(&response) {
        Ok(frame) => frame,
        Err(_) => return respond(&mut stream, 500, "Internal Server Error", b""),
    };
    // 租约失效或本机执行超时：都不登记完成，避免把放弃/失败写成权威结果。
    if !aborted_now
        && ledger_owned
        && let Some(endpoint) = config.ledger.as_ref()
    {
        // 执行后必须用当前持有者令牌登记；令牌失效时回 Aborted + 标记，让调度方
        // 等待当前持有者的结果，而不是继续返回本机的旧结果。
        let Some(lease) = ledger_lease.as_deref() else {
            return respond(&mut stream, 503, "Service Unavailable", b"");
        };
        match remote_ledger::complete_checked(endpoint, &key, lease, &frame) {
            Ok(()) => {}
            Err(remote_ledger::CompleteError::LeaseLost) => {
                return respond_aborted_lease_lost(&mut stream, &request);
            }
            Err(_) => return respond(&mut stream, 503, "Service Unavailable", b""),
        }
    }
    if dedupe
        && !ledger_owned
        && !timed_out_now
        && let Ok(mut guard) = cache.lock()
    {
        guard.insert(key, frame.clone(), config.max_cache_bytes, config.cache_ttl);
    }
    drop(_gate_guard);
    if lease_lost_now {
        return respond_lease_lost(&mut stream, &frame);
    }
    if timed_out_now {
        return respond_deadline_exceeded(&mut stream, &frame);
    }
    respond(&mut stream, 200, "OK", &frame)
}

/// 把 `{task}` / `{workspace}` 占位符按请求展开；配了 workspace 根时先建目录。
/// 有计划作用域时目录为 `<root>/<scope>/<task>`，否则保持 `<root>/<task>`。
fn exec_for_request(
    config: &SubagentProcessConfig,
    workspace_root: Option<&Path>,
    task: &str,
    scope: Option<&str>,
) -> Result<SubagentProcessConfig, String> {
    let uses_task = config.args.iter().any(|arg| arg.contains("{task}"));
    let uses_workspace = config.args.iter().any(|arg| arg.contains("{workspace}"));
    if !uses_task && !uses_workspace && workspace_root.is_none() {
        return Ok(config.clone());
    }
    let component = safe_task_component(task)?;
    let scope_component = scope.map(safe_scope_component).transpose()?;
    let workspace = workspace_root.map(|root| {
        let mut path = root.to_path_buf();
        if let Some(scope) = scope_component.as_deref() {
            path.push(scope);
        }
        path.push(&component);
        path
    });
    if uses_workspace && workspace.is_none() {
        return Err("exec args use {workspace} but no workspace root is configured".into());
    }
    if let Some(workspace) = workspace.as_deref() {
        std::fs::create_dir_all(workspace).map_err(|error| {
            format!("create per-task workspace {}: {error}", workspace.display())
        })?;
    }
    let mut materialized = config.clone();
    materialized.args = config
        .args
        .iter()
        .map(|arg| {
            let mut arg = arg.replace("{task}", &component);
            if let Some(workspace) = workspace.as_deref() {
                arg = arg.replace("{workspace}", &workspace.to_string_lossy());
            }
            arg
        })
        .collect();
    Ok(materialized)
}

/// 只允许单段、无路径分隔符的任务 ID 作为目录名，拒绝 `.` / `..`。
fn safe_task_component(task: &str) -> Result<String, String> {
    if task.is_empty() || task == "." || task == ".." {
        return Err(format!(
            "task id {task:?} is not a safe workspace directory"
        ));
    }
    if !task
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(format!(
            "task id {task:?} contains characters unsafe for a workspace directory"
        ));
    }
    Ok(task.to_string())
}

/// 校验从幂等键里解析出的计划作用域，作为路径段使用。
fn safe_scope_component(scope: &str) -> Result<String, String> {
    if scope.len() > crate::remote_subagent::MAX_IDEMPOTENCY_SCOPE_LEN
        || !crate::remote_subagent::is_safe_namespace_component(scope)
    {
        return Err(format!(
            "idempotency scope {scope:?} is not a safe workspace directory"
        ));
    }
    Ok(scope.to_string())
}

/// 分片睡眠，取消置位时立即返回 `true`。
fn sleep_unless_cancelled(ms: u64, token: Option<&CancelToken>) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        if token.is_some_and(CancelToken::is_cancelled) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep((deadline - now).min(CANCEL_CHECK_STEP));
    }
}

/// 未开始执行就被取消：回一个合法 `Aborted` 帧，而不是空体。
fn respond_aborted(stream: &mut TcpStream, request: &SubagentRequest) -> std::io::Result<()> {
    let frame = aborted_frame(request)?;
    respond(stream, 200, "OK", &frame)
}

/// 租约在完成登记时才发现失效：同样回 `Aborted`，但带“不是节点故障”的标记。
fn respond_aborted_lease_lost(
    stream: &mut TcpStream,
    request: &SubagentRequest,
) -> std::io::Result<()> {
    let frame = aborted_frame(request)?;
    respond_lease_lost(stream, &frame)
}

/// 执行时限在账本等待期到点：回带超预算标记的合法 `Aborted`。
fn respond_aborted_deadline(
    stream: &mut TcpStream,
    request: &SubagentRequest,
) -> std::io::Result<()> {
    let frame = aborted_frame(request)?;
    respond_deadline_exceeded(stream, &frame)
}

fn aborted_frame(request: &SubagentRequest) -> std::io::Result<Vec<u8>> {
    let response = SubagentResponse {
        task: request.task.clone(),
        epoch: request.epoch,
        status: DelegationStatus::Aborted,
        content: Vec::new(),
        tokens: 0,
    };
    encode_frame(&response).map_err(std::io::Error::other)
}

/// 租约被接管后返回合法 `Aborted` 帧，并标记“不是节点故障”。
fn respond_lease_lost(stream: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    respond_flagged_aborted(stream, body, LEASE_LOST_HEADER)
}

/// 执行时限到点后返回合法 `Aborted` 帧，并标记“节点健康、请求超预算”。
fn respond_deadline_exceeded(stream: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    respond_flagged_aborted(stream, body, DEADLINE_EXCEEDED_HEADER)
}

fn respond_flagged_aborted(
    stream: &mut TcpStream,
    body: &[u8],
    header: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n{header}: 1\r\nConnection: close\r\n\r\n",
        crate::remote_subagent::FRAME_CONTENT_TYPE,
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

/// 写一个带长度与 `Connection: close` 的响应。
fn respond(stream: &mut TcpStream, code: u16, reason: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        crate::remote_subagent::FRAME_CONTENT_TYPE,
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}
