//! Bounded, authenticated loopback RPC. Each connection has a fresh challenge;
//! encrypted requests and replies are bound to it and to their direction.
//! Accepted actions outlive a disconnected caller and retain their outcome.

use super::{Action, Reply, Request};
use chacha20poly1305::{
    aead::{Aead, Payload},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{watch, Semaphore},
};

const MAX_FRAME: usize = 64 * 1024;
const MAX_RECORDS: usize = 512;
const RETAIN_MS: u64 = 15 * 60 * 1000;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const ACTION_TIMEOUT: Duration = Duration::from_secs(660);

pub(crate) type Executor =
    Arc<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Reply> + Send>> + Send + Sync>;

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn request(guild: u64, user: u64, action: Action) -> Request {
    Request {
        id: uuid::Uuid::new_v4().to_string(),
        expires: now_ms() + 60_000,
        guild,
        user,
        room: None,
        target: None,
        action,
    }
}

fn invalid() -> io::Error {
    io::Error::other("invalid or unauthenticated routing message")
}

fn seal(key: &[u8; 32], challenge: &[u8; 32], direction: u8, body: &[u8]) -> io::Result<Vec<u8>> {
    let nonce = rand::random::<[u8; 24]>();
    let mut aad = challenge.to_vec();
    aad.push(direction);
    let encrypted = XChaCha20Poly1305::new(key.into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: body,
                aad: &aad,
            },
        )
        .map_err(|_| invalid())?;
    let mut out = nonce.to_vec();
    out.extend(encrypted);
    if out.len() > MAX_FRAME {
        return Err(invalid());
    }
    Ok(out)
}

fn unseal(
    key: &[u8; 32],
    challenge: &[u8; 32],
    direction: u8,
    frame: &[u8],
) -> io::Result<Vec<u8>> {
    if frame.len() < 40 || frame.len() > MAX_FRAME {
        return Err(invalid());
    }
    let mut aad = challenge.to_vec();
    aad.push(direction);
    XChaCha20Poly1305::new(key.into())
        .decrypt(
            XNonce::from_slice(&frame[..24]),
            Payload {
                msg: &frame[24..],
                aad: &aad,
            },
        )
        .map_err(|_| invalid())
}

async fn read_frame(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let len = stream.read_u32().await? as usize;
    if !(40..=MAX_FRAME).contains(&len) {
        return Err(invalid());
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn write_frame(stream: &mut TcpStream, bytes: &[u8]) -> io::Result<()> {
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}

#[derive(Clone)]
pub(crate) struct Client {
    pub address: SocketAddr,
    key: [u8; 32],
}

impl Client {
    pub fn new(address: SocketAddr, key: [u8; 32]) -> Self {
        Self { address, key }
    }

    pub async fn call(&self, request: &Request) -> io::Result<Reply> {
        let body = serde_json::to_vec(request).map_err(|_| invalid())?;
        let mut stream = tokio::time::timeout(IO_TIMEOUT, async {
            let mut stream = TcpStream::connect(self.address).await?;
            let mut challenge = [0u8; 32];
            stream.read_exact(&mut challenge).await?;
            let frame = seal(&self.key, &challenge, 0, &body)?;
            write_frame(&mut stream, &frame).await?;
            Ok::<_, io::Error>((stream, challenge))
        })
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        let wait = if matches!(request.action, Action::FinishLogin { .. }) {
            ACTION_TIMEOUT + IO_TIMEOUT
        } else {
            Duration::from_secs(65)
        };
        let frame = tokio::time::timeout(wait, read_frame(&mut stream.0))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        let plain = unseal(&self.key, &stream.1, 1, &frame)?;
        serde_json::from_slice(&plain).map_err(|_| invalid())
    }
}

struct Record {
    fingerprint: [u8; 32],
    user: u64,
    guild: u64,
    until: u64,
    result: watch::Receiver<Option<Reply>>,
}

#[derive(Default)]
struct Journal {
    records: Mutex<HashMap<String, Record>>,
}

impl Journal {
    async fn run(&self, request: Request, execute: &Executor) -> Reply {
        let now = now_ms();
        if request.user == 0
            || request.guild == 0
            || uuid::Uuid::parse_str(&request.id).is_err()
            || request.expires < now
            || request.expires > now + 60_000
        {
            return Reply::Error("This request expired. Open a fresh menu.".into());
        }
        if let Action::Result { request: id } = &request.action {
            let records = self.records.lock();
            return records
                .get(id)
                .filter(|record| {
                    record.user == request.user
                        && record.guild == request.guild
                        && record.until > now
                })
                .map(|record| {
                    record.result.borrow().clone().unwrap_or_else(|| {
                        if record.result.has_changed().is_err() {
                            Reply::Error(
                                "Request outcome was lost. Check playback before retrying.".into(),
                            )
                        } else {
                            Reply::Pending
                        }
                    })
                })
                .unwrap_or_else(|| {
                    Reply::Error(
                        "That request outcome is unavailable; check playback before retrying."
                            .into(),
                    )
                });
        }
        if matches!(request.action, Action::Status) {
            return tokio::time::timeout(IO_TIMEOUT, execute(request))
                .await
                .unwrap_or_else(|_| Reply::Error("This bot is not responding.".into()));
        }
        let fingerprint: [u8; 32] =
            Sha256::digest(serde_json::to_vec(&request).expect("serializable request")).into();
        let mut result = {
            let mut records = self.records.lock();
            records.retain(|_, record| record.until > now);
            if let Some(record) = records.get(&request.id) {
                if record.fingerprint != fingerprint {
                    return Reply::Error("Request identity conflict.".into());
                }
                record.result.clone()
            } else {
                if records.len() >= MAX_RECORDS {
                    return Reply::Error("Request history is full. Try again shortly.".into());
                }
                let (tx, rx) = watch::channel(None);
                records.insert(
                    request.id.clone(),
                    Record {
                        fingerprint,
                        user: request.user,
                        guild: request.guild,
                        until: now + RETAIN_MS,
                        result: rx.clone(),
                    },
                );
                let execute = execute.clone();
                tokio::spawn(async move {
                    let limit = if matches!(request.action, Action::FinishLogin { .. }) {
                        ACTION_TIMEOUT
                    } else {
                        Duration::from_secs(60)
                    };
                    let reply = tokio::time::timeout(limit, execute(request))
                        .await
                        .unwrap_or_else(|_| {
                            Reply::Error(
                                "Request timed out; check playback before retrying.".into(),
                            )
                        });
                    let reply = match reply {
                        Reply::Text(text) => Reply::Text(text.chars().take(1900).collect()),
                        Reply::Error(text) => Reply::Error(text.chars().take(1900).collect()),
                        reply => reply,
                    };
                    let _ = tx.send(Some(reply));
                });
                rx
            }
        };
        loop {
            if let Some(reply) = result.borrow_and_update().clone() {
                return reply;
            }
            if result.changed().await.is_err() {
                return Reply::Error(
                    "Request outcome was lost. Check playback before retrying.".into(),
                );
            }
        }
    }
}

/// Bind before startup reports success. No public interfaces are accepted.
pub(crate) async fn listen(
    address: SocketAddr,
    key: [u8; 32],
    execute: Executor,
) -> io::Result<SocketAddr> {
    if !address.ip().is_loopback() {
        return Err(invalid());
    }
    let listener = TcpListener::bind(address).await?;
    let address = listener.local_addr()?;
    let capacity = Arc::new(Semaphore::new(16));
    let journal = Arc::new(Journal::default());
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let Ok(permit) = capacity.clone().try_acquire_owned() else {
                continue;
            };
            let execute = execute.clone();
            let journal = journal.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let challenge = rand::random::<[u8; 32]>();
                let parsed = tokio::time::timeout(IO_TIMEOUT, async {
                    stream.write_all(&challenge).await?;
                    let frame = read_frame(&mut stream).await?;
                    let bytes = unseal(&key, &challenge, 0, &frame)?;
                    serde_json::from_slice::<Request>(&bytes).map_err(|_| invalid())
                })
                .await;
                let Ok(Ok(request)) = parsed else {
                    return;
                };
                let reply = journal.run(request, &execute).await;
                let Ok(body) = serde_json::to_vec(&reply) else {
                    return;
                };
                let Ok(frame) = seal(&key, &challenge, 1, &body) else {
                    return;
                };
                let _ = tokio::time::timeout(IO_TIMEOUT, write_frame(&mut stream, &frame)).await;
            });
        }
    });
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn messages_are_bound_to_key_connection_and_direction() {
        let frame = seal(&[1; 32], &[2; 32], 0, b"request").unwrap();
        assert_eq!(unseal(&[1; 32], &[2; 32], 0, &frame).unwrap(), b"request");
        assert!(unseal(&[3; 32], &[2; 32], 0, &frame).is_err());
        assert!(unseal(&[1; 32], &[3; 32], 0, &frame).is_err());
        assert!(unseal(&[1; 32], &[2; 32], 1, &frame).is_err());
    }

    #[tokio::test]
    async fn real_connection_retries_execute_once_and_reject_wrong_key() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let execute: Executor = Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Reply::Text("done".into())
            })
        });
        let address = listen("127.0.0.1:0".parse().unwrap(), [1; 32], execute)
            .await
            .unwrap();
        let client = Client::new(address, [1; 32]);
        let req = request(1, 2, Action::Logout);
        let (a, b) = tokio::join!(client.call(&req), client.call(&req));
        assert!(matches!(a.unwrap(), Reply::Text(_)));
        assert!(matches!(b.unwrap(), Reply::Text(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(Client::new(address, [2; 32]).call(&req).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let mut conflicting = req.clone();
        conflicting.action = Action::Forget;
        assert!(matches!(
            client.call(&conflicting).await.unwrap(),
            Reply::Error(_)
        ));
        let mut expired = request(1, 2, Action::Logout);
        expired.expires = now_ms() - 1;
        assert!(matches!(
            client.call(&expired).await.unwrap(),
            Reply::Error(_)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn disconnected_waiter_can_query_its_own_outcome() {
        let journal = Arc::new(Journal::default());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let execute: Executor = {
            let entered = entered.clone();
            let release = release.clone();
            let calls = calls.clone();
            Arc::new(move |_| {
                let entered = entered.clone();
                let release = release.clone();
                let calls = calls.clone();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    release.notified().await;
                    Reply::Text("complete".into())
                })
            })
        };
        let req = request(1, 2, Action::Forget);
        let waiter = {
            let journal = journal.clone();
            let execute = execute.clone();
            let req = req.clone();
            tokio::spawn(async move { journal.run(req, &execute).await })
        };
        entered.notified().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        let pending = request(
            1,
            2,
            Action::Result {
                request: req.id.clone(),
            },
        );
        assert!(matches!(
            journal.run(pending, &execute).await,
            Reply::Pending
        ));
        release.notify_one();
        // A retry subscribes to the original execution, even with its first
        // response waiter gone. It cannot execute Forget a second time.
        assert!(matches!(
            journal.run(req.clone(), &execute).await,
            Reply::Text(_)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let query = request(
            1,
            2,
            Action::Result {
                request: req.id.clone(),
            },
        );
        assert!(matches!(journal.run(query, &execute).await, Reply::Text(_)));
        let query = request(1, 3, Action::Result { request: req.id });
        assert!(matches!(
            journal.run(query, &execute).await,
            Reply::Error(_)
        ));
    }
}
