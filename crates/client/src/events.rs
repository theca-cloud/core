//! The event feed (`POST /events`, Server-Sent Events), for native clients:
//! typed messages, the positions kept, and reconnection. A browser has
//! `EventSource`; the web app doesn't read the feed.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::Duration;

use futures::{Stream, StreamExt};
use reqwest::Method;
use theca_common::api;
use uuid::Uuid;

use crate::api::{Client, Error, decode};

/// A message of the event feed.
#[derive(Debug, Clone)]
pub enum FeedMessage {
    /// Every library the credential can access, at the start of each
    /// connection and whenever that changes.
    Libraries(Vec<api::FeedLibrary>),
    /// A batch of changes, collapsed per resource, with the new positions.
    Changes(api::Changes),
}

/// The longest wait between two reconnections.
const MAX_DELAY: Duration = Duration::from_secs(30);

type Body = Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

struct State {
    client: Client,
    /// The last event received in each library followed.
    positions: BTreeMap<Uuid, api::Position>,
    /// The response's body, and what arrived of the next message (bytes: a
    /// character can be split across two chunks).
    connection: Option<(Body, Vec<u8>)>,
    delay: Duration,
    over: bool,
}

impl Client {
    /// The event feed from `after` (empty on a first launch), then live. Cut,
    /// it reconnects from the last positions received, waiting longer each
    /// time (up to 30 s). It ends after an error that waiting won't fix (the
    /// credential refused), given as its last item.
    pub fn events(
        &self,
        after: BTreeMap<Uuid, api::Position>,
    ) -> impl Stream<Item = Result<FeedMessage, Error>> + Send + use<> {
        let start = State {
            client: self.clone(),
            positions: after,
            connection: None,
            delay: Duration::from_millis(500),
            over: false,
        };
        futures::stream::unfold(start, |mut s| async move {
            if s.over {
                return None;
            }
            loop {
                let Some((bytes, buffer)) = &mut s.connection else {
                    match connect(&s.client, &s.positions).await {
                        Ok(connection) => {
                            s.connection = Some(connection);
                            s.delay = Duration::from_millis(500);
                        }
                        Err(e) if e.transient() => s.wait().await,
                        Err(e) => {
                            s.over = true;
                            return Some((Err(e), s));
                        }
                    }
                    continue;
                };
                if let Some(frame) = next_frame(buffer) {
                    match parse(&frame) {
                        Ok(Some(message)) => {
                            s.follow(&message);
                            return Some((Ok(message), s));
                        }
                        Ok(None) => continue,
                        Err(e) => {
                            s.over = true;
                            return Some((Err(e), s));
                        }
                    }
                }
                match bytes.next().await {
                    Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                    // Cut (or ended by the server): again, from the positions.
                    Some(Err(_)) | None => {
                        s.connection = None;
                        s.wait().await;
                    }
                }
            }
        })
    }
}

/// Opens the stream from `positions`.
async fn connect(
    client: &Client,
    positions: &BTreeMap<Uuid, api::Position>,
) -> Result<(Body, Vec<u8>), Error> {
    let body = serde_json::to_vec(&api::Events {
        after: positions.clone(),
    })
    .map_err(|e| Error::Decode(e.to_string()))?;
    let response = client
        .request(Method::POST, "/api/v1/events")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(decode::<serde_json::Value>(response)
            .await
            .err()
            .unwrap_or(Error::Decode("an error without an error".into())));
    }
    Ok((Box::pin(response.bytes_stream()), Vec::new()))
}

impl State {
    async fn wait(&mut self) {
        crate::rt::sleep(self.delay).await;
        self.delay = (self.delay * 2).min(MAX_DELAY);
    }

    /// The positions a reconnection sends: a library announced is followed
    /// from its position (unless already followed), one no longer listed
    /// dropped; a batch moves its libraries'.
    fn follow(&mut self, message: &FeedMessage) {
        match message {
            FeedMessage::Libraries(libraries) => {
                self.positions = libraries
                    .iter()
                    .map(|l| match self.positions.get(&l.id) {
                        Some(&p) if !l.reset => (l.id, p),
                        _ => (
                            l.id,
                            api::Position::Event {
                                seq: l.position,
                                event: l.event,
                            },
                        ),
                    })
                    .collect();
            }
            FeedMessage::Changes(changes) => {
                for (library, &seq) in &changes.positions {
                    let event = changes.events.get(library).copied();
                    self.positions
                        .insert(*library, api::Position::Event { seq, event });
                }
            }
        }
    }
}

/// The next complete SSE frame (up to its blank line), taken out of `buffer`.
fn next_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let end = buffer.windows(2).position(|w| w == b"\n\n")?;
    Some(buffer.drain(..end + 2).collect())
}

/// One SSE frame: a message, or `None` for a comment (the server's pings) or
/// an event this version doesn't know.
fn parse(frame: &[u8]) -> Result<Option<FeedMessage>, Error> {
    let frame = std::str::from_utf8(frame).map_err(|e| Error::Decode(e.to_string()))?;
    let (mut event, mut data) = (None, Vec::new());
    for line in frame.lines() {
        if let Some(e) = line.strip_prefix("event:") {
            event = Some(e.trim_start());
        } else if let Some(d) = line.strip_prefix("data:") {
            data.push(d.strip_prefix(' ').unwrap_or(d));
        }
    }
    let data = data.join("\n");
    let decode = |e: serde_json::Error| Error::Decode(e.to_string());
    Ok(match event {
        Some("libraries") => Some(FeedMessage::Libraries(
            serde_json::from_str(&data).map_err(decode)?,
        )),
        Some("changes") => Some(FeedMessage::Changes(
            serde_json::from_str(&data).map_err(decode)?,
        )),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        assert!(parse(b":\n\n").unwrap().is_none());
        assert!(parse(b"event: later\ndata: {}\n\n").unwrap().is_none());
        // Several data lines are one value, joined by line feeds.
        let frame = b"event: changes\ndata: {\"entries\":[],\ndata: \"positions\":{}}\n\n";
        let Some(FeedMessage::Changes(c)) = parse(frame).unwrap() else {
            panic!("changes")
        };
        assert!(c.entries.is_empty());
        assert!(parse(b"event: changes\ndata: {\n\n").is_err());
    }

    #[test]
    fn a_character_split_across_chunks() {
        let message = "event: libraries\ndata: [{\"id\":\"00000000-0000-0000-0000-000000000001\",\"organization\":\"00000000-0000-0000-0000-000000000002\",\"name\":\"\u{c9}t\u{e9}\",\"root\":\"00000000-0000-0000-0000-000000000003\",\"position\":4}]\n\n";
        let cut = message.find('\u{c9}').unwrap() + 1;
        let mut buffer = message.as_bytes()[..cut].to_vec();
        assert!(next_frame(&mut buffer).is_none());
        buffer.extend_from_slice(&message.as_bytes()[cut..]);
        let frame = next_frame(&mut buffer).unwrap();
        let Some(FeedMessage::Libraries(l)) = parse(&frame).unwrap() else {
            panic!("libraries")
        };
        assert_eq!(l[0].name, "\u{c9}t\u{e9}");
        assert!(buffer.is_empty());
    }
}
