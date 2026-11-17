//! 短时保留刚用过的 SSH 会话，让紧接着的下一步直接复用。
//!
//! 一次完整的 SSH 连接（TCP、密钥交换、认证）大约十几个网络往返，高延迟链路上要好几秒，
//! 经跳板机还要翻倍。而用户的一次操作往往连续触发好几个需要连接的步骤：
//! 测试连接 → 预检 → 开始传输；浏览远端目录时每点一层都是一次；批量传输更是每个文件预检一次、
//! 传输一次。逐次重连时，用户感受到的就是「点了没反应」。
//!
//! 这里只留一个空闲会话（应用同一时间只面向一台服务器），按连接配置的摘要匹配：
//! 主机、用户、凭据或跳板机链任何一项不同都不会复用。空闲超过 IDLE_TTL 自动断开。

use super::client::{
    connect_session, hurry_failed_session, set_operation_timeout, ConnectError, ConnectionConfig, DEFAULT_OPERATION_TIMEOUT_MS,
};
use sha2::{Digest, Sha256};
use ssh2::Session;
use std::net::TcpStream;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

/// 空闲会话最多保留多久。要覆盖「看完预检结果、点确认」的间隔，又不能让一条没人用的连接
/// 长时间挂在服务器上；也必须远小于跳板机转发线程的空闲上限，否则会被它先掐断。
const IDLE_TTL: Duration = Duration::from_secs(45);
/// 复用前探活的超时。链路已经悄悄断掉（NAT 超时、切换网络）时不能按常规的 30 秒去等，
/// 那比重新连接还慢；探活失败就当它不存在，照常新建连接。
const LIVENESS_TIMEOUT_MS: u64 = 3_000;

struct Idle {
    key: [u8; 32],
    session: Session,
    abort: TcpStream,
    stored_at: Instant,
    /// 每次存入递增，过期清理线程据此判断自己要清的那一份是否还在。
    generation: u64,
}

struct Pool {
    idle: Option<Idle>,
    next_generation: u64,
}

static POOL: Mutex<Pool> = Mutex::new(Pool { idle: None, next_generation: 0 });

fn lock() -> std::sync::MutexGuard<'static, Pool> {
    // 持锁期间只做取放，不会 panic；即便锁中毒，里面的数据也仍然一致。
    POOL.lock().unwrap_or_else(|e| e.into_inner())
}

/// 连接配置的摘要。只存摘要而不存配置本身：池不应该成为又一个长期持有密码的地方。
fn config_key(config: &ConnectionConfig) -> [u8; 32] {
    fn feed(hasher: &mut Sha256, config: &ConnectionConfig) {
        // Debug 输出包含全部字段（含凭据），字段之间有结构分隔，不会因拼接产生歧义。
        hasher.update(format!("{:?}|{}|{:?}|{:?}", config.host, config.port, config.username, config.auth).as_bytes());
        if let Some(jump) = &config.jump {
            hasher.update(b"|via|");
            feed(hasher, jump);
        }
    }
    let mut hasher = Sha256::new();
    feed(&mut hasher, config);
    hasher.finalize().into()
}

/// 会话是否还通：开一个通道再关掉，约两个往返。
fn is_alive(session: &Session) -> bool {
    set_operation_timeout(session, LIVENESS_TIMEOUT_MS);
    let alive = session.channel_session().is_ok();
    set_operation_timeout(session, DEFAULT_OPERATION_TIMEOUT_MS);
    alive
}

/// 取出与该配置匹配、未过期且探活通过的空闲会话。
fn take(config: &ConnectionConfig) -> Option<(Session, TcpStream)> {
    let key = config_key(config);
    let idle = {
        let mut pool = lock();
        match &pool.idle {
            Some(idle) if idle.key == key => pool.idle.take(),
            _ => None,
        }
    }?;
    // 探活要走网络，必须在释放锁之后做。
    if idle.stored_at.elapsed() > IDLE_TTL || !is_alive(&idle.session) {
        log::debug!("pooled session for {} is stale, reconnecting", config.endpoint());
        return None;
    }
    log::info!("reusing warm session for {}", config.endpoint());
    Some((idle.session, idle.abort))
}

/// 复用空闲会话，没有可用的就新建连接。返回值与 `connect_session` 相同。
pub fn acquire(config: &ConnectionConfig) -> Result<(Session, TcpStream), ConnectError> {
    match take(config) {
        Some(warm) => Ok(warm),
        None => connect_session(config),
    }
}

/// 把仍然完好的会话交还，供下一步复用。只应在操作成功后调用：出过错的会话可能已经断了。
pub fn release(config: &ConnectionConfig, session: Session, abort: TcpStream) {
    let (previous, generation) = {
        let mut pool = lock();
        let generation = pool.next_generation;
        pool.next_generation += 1;
        let idle = Idle { key: config_key(config), session, abort, stored_at: Instant::now(), generation };
        (pool.idle.replace(idle), generation)
    };
    // 被顶替的旧会话在锁外析构：断开连接要发包，不能卡住其他取放。
    drop(previous);

    let spawned = thread::Builder::new().name("ssh-pool-expiry".into()).spawn(move || {
        thread::sleep(IDLE_TTL);
        let expired = {
            let mut pool = lock();
            match &pool.idle {
                Some(idle) if idle.generation == generation => pool.idle.take(),
                _ => None,
            }
        };
        if expired.is_some() {
            log::debug!("closing idle pooled session");
        }
    });
    if let Err(e) = spawned {
        // 起不了清理线程时宁可不留会话，也不能让连接无限期挂着。
        log::warn!("cannot start pool expiry thread, dropping session: {e}");
        clear();
    }
}

/// 在复用（或新建）的会话上执行一个短操作，成功后把会话留给下一步。
pub fn with_session<T>(
    config: &ConnectionConfig,
    operation: impl FnOnce(&Session) -> Result<T, String>,
) -> Result<T, String> {
    let (session, abort) = acquire(config).map_err(|e| e.to_string())?;
    let result = operation(&session);
    if result.is_ok() {
        release(config, session, abort);
    } else {
        // 出错的会话不再留用；先让它的收尾尽快结束，免得调用方在一条坏连接上多等一两次超时。
        hurry_failed_session(&session);
    }
    result
}

/// 立即断开空闲会话（测试用；也用于清理线程起不来时的兜底）。
pub fn clear() {
    let idle = lock().idle.take();
    drop(idle);
}

/// 当前是否留有空闲会话（端到端测试用来确认复用确实发生）。
#[cfg(test)]
pub fn has_idle() -> bool {
    lock().idle.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sftp::AuthMethod;

    fn config(password: &str, jump: Option<ConnectionConfig>) -> ConnectionConfig {
        ConnectionConfig {
            host: "example.com".into(),
            port: 22,
            username: "deploy".into(),
            auth: AuthMethod::Password(password.into()),
            jump: jump.map(Box::new),
        }
    }

    #[test]
    fn key_changes_with_any_connection_detail() {
        let base = config_key(&config("a", None));
        assert_eq!(base, config_key(&config("a", None)));
        assert_ne!(base, config_key(&config("b", None)), "credentials must be part of the key");
        assert_ne!(base, config_key(&config("a", Some(config("a", None)))), "jump chain must be part of the key");
    }
}
