//! Lighter closes clients that send no frame for two minutes, even while
//! market data keeps arriving. Receiving traffic must not postpone our ping.
//! https://apidocs.lighter.xyz/docs/websocket-reference#keepalive-requirements
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_tungstenite::tungstenite::{Error, Message};

pub(crate) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const WRITE_DEADLINE: Duration = Duration::from_secs(5);

pub(crate) struct LighterKeepalive {
    timer: Interval,
    pub sent: u64,
}

impl LighterKeepalive {
    pub fn new() -> Self {
        let mut timer = tokio::time::interval_at(Instant::now() + KEEPALIVE_INTERVAL, KEEPALIVE_INTERVAL);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Self { timer, sent: 0 }
    }

    // No detached task: reconnect/drop cancels the old connection's heartbeat.
    // Callers retain their receive deadline and book freshness/nonce checks.
    // Sending a ping never produces a market or account observation.
    pub async fn next<R, W>(&mut self, reader: &mut R, writer: &mut W) -> Result<Option<Message>>
    where
        R: Stream<Item = std::result::Result<Message, Error>> + Unpin,
        W: Sink<Message, Error = Error> + Unpin,
    {
        loop {
            tokio::select! {
                biased;
                _ = self.timer.tick() => {
                    tokio::time::timeout(WRITE_DEADLINE,
                        writer.send(Message::Text(r#"{"type":"ping"}"#.into())))
                        .await.context("Lighter keepalive write deadline exceeded")?
                        .context("Lighter keepalive write failed")?;
                    self.sent += 1;
                }
                message = reader.next() => return message.transpose().context("Lighter websocket read failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{pin::Pin, task::{Context as TaskContext, Poll}};
    use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};

    // Exercise real websocket framing, while virtual time covers several
    // two-minute venue idle windows without slowing the release tests.
    #[tokio::test(start_paused = true)]
    async fn active_inbound_market_traffic_does_not_starve_keepalive() {
        let (a, b) = tokio::io::duplex(4096);
        let client = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(b, Role::Server, None).await;
        let task = tokio::spawn(async move {
            let (mut writer, mut reader) = client.split();
            let mut heartbeat = LighterKeepalive::new();
            let mut books = 0;
            let mut pongs = 0;
            while let Some(message) = heartbeat.next(&mut reader, &mut writer).await.unwrap() {
                match message {
                    Message::Text(text) if text == "book" => books += 1,
                    Message::Text(text) if text == r#"{"type":"pong"}"# => pongs += 1,
                    Message::Close(_) => break,
                    _ => {},
                }
            }
            (heartbeat.sent, books, pongs)
        });
        tokio::task::yield_now().await;
        for second in (10..=300).step_by(10) {
            tokio::time::advance(Duration::from_secs(10)).await;
            server.send(Message::Text("book".into())).await.unwrap();
            if second % 30 == 0 {
                assert_eq!(server.next().await.unwrap().unwrap(), Message::Text(r#"{"type":"ping"}"#.into()));
                server.send(Message::Text(r#"{"type":"pong"}"#.into())).await.unwrap();
            }
            tokio::task::yield_now().await;
        }
        server.close(None).await.unwrap();
        assert_eq!(task.await.unwrap(), (10, 30, 10));
    }

    #[tokio::test(start_paused = true)]
    async fn outbound_pings_do_not_extend_receive_deadline() {
        let mut heartbeat = LighterKeepalive::new();
        let mut reader = futures_util::stream::pending::<std::result::Result<Message, Error>>();
        let mut writer = futures_util::sink::drain().sink_map_err(|never| match never {});
        let start = Instant::now();
        assert!(tokio::time::timeout(Duration::from_secs(65), heartbeat.next(&mut reader, &mut writer)).await.is_err());
        assert_eq!(start.elapsed(), Duration::from_secs(65));
        assert_eq!(heartbeat.sent, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn missed_ticks_do_not_burst_after_a_stall() {
        let mut heartbeat = LighterKeepalive::new();
        let mut reader = futures_util::stream::iter([Ok(Message::Pong(vec![])), Ok(Message::Pong(vec![]))]);
        let mut writer = futures_util::sink::drain().sink_map_err(|never| match never {});
        tokio::time::advance(Duration::from_secs(300)).await;
        for _ in 0..2 {
            assert!(heartbeat.next(&mut reader, &mut writer).await.unwrap().is_some());
            assert_eq!(heartbeat.sent, 1);
        }
    }

    struct BlockedWriter;
    impl Sink<Message> for BlockedWriter {
        type Error = Error;
        fn poll_ready(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::result::Result<(), Error>> { Poll::Pending }
        fn start_send(self: Pin<&mut Self>, _: Message) -> std::result::Result<(), Error> { unreachable!() }
        fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::result::Result<(), Error>> { Poll::Pending }
        fn poll_close(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::result::Result<(), Error>> { Poll::Ready(Ok(())) }
    }

    #[tokio::test(start_paused = true)]
    async fn stuck_ping_write_exits_in_five_seconds() {
        let mut heartbeat = LighterKeepalive::new();
        let mut reader = futures_util::stream::pending::<std::result::Result<Message, Error>>();
        let start = Instant::now();
        let error = heartbeat.next(&mut reader, &mut BlockedWriter).await.unwrap_err();
        assert!(error.to_string().contains("write deadline exceeded"));
        assert_eq!(start.elapsed(), KEEPALIVE_INTERVAL + WRITE_DEADLINE);
        assert_eq!(heartbeat.sent, 0);
    }
}
