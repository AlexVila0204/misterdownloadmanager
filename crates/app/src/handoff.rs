//! The browser's own download, looked at before the browser lets it go (#630). The browser has
//! asked the address already when it hands the download over; a one-time link (a game panel's
//! file download, a signed token) answers a second request 404. So the app asks it once, as the
//! download will, and answers the browser's host only then: `spent` when the address no longer
//! gives the file (the browser goes on with the answer it holds), `ok` otherwise, the open
//! answer kept for the download to read. A look that takes longer than the host waits is
//! answered `ok` at once and read when it comes, as a dialog's look is.

use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::dialogs::Looked;

/// How long the browser's host is kept waiting: inside the channel's own wait, which says
/// `timeout` (taken) after it.
const ANSWER_WITHIN: Duration = mdm_ipc::REPLY_WAIT.saturating_sub(Duration::from_millis(500));

type Answer = Arc<Mutex<Option<Result<Looked, mdm_engine::Error>>>>;

/// One hand-over being looked at.
pub struct Look {
    cmd: mdm_ipc::Command,
    reply: Option<SyncSender<String>>,
    answer: Answer,
    asked: Instant,
}

/// How a look ended.
pub enum Settled {
    /// The address no longer gives the file: the browser keeps its download.
    Spent,
    /// The app takes the download, with what the look learned when it can be read.
    Take(Box<(mdm_ipc::Command, Option<Looked>)>),
}

impl Look {
    pub fn start(
        cmd: mdm_ipc::Command,
        reply: SyncSender<String>,
        url: url::Url,
        request: &crate::app::RequestParts,
        rt: &tokio::runtime::Handle,
        ctx: &egui::Context,
    ) -> Look {
        let answer = Answer::default();
        let (slot, woken) = (answer.clone(), ctx.clone());
        let (headers, opts) = (request.headers.clone(), request.opts.clone());
        rt.spawn(async move {
            let res = mdm_engine::probe_kept(url, headers, &opts).await;
            *slot.lock().unwrap() = Some(res);
            woken.request_repaint();
        });
        // Woken to answer the host in time when the server is slow.
        ctx.request_repaint_after(ANSWER_WITHIN);
        Look {
            cmd,
            reply: Some(reply),
            answer,
            asked: Instant::now(),
        }
    }

    /// The host answered when it is due; how the look ended, once it has.
    pub fn poll(&mut self) -> Option<Settled> {
        let res = self.answer.lock().unwrap().take();
        match res {
            None => {
                if self.asked.elapsed() >= ANSWER_WITHIN {
                    self.say("ok");
                }
                None
            }
            Some(Err(e)) if e.wants_new_address() && self.reply.is_some() => {
                self.say(mdm_ipc::SPENT);
                tracing::info!("browser hand-off left to the browser, its address spent: {e}");
                Some(Settled::Spent)
            }
            Some(res) => {
                self.say("ok");
                let cmd = std::mem::take(&mut self.cmd);
                Some(Settled::Take(Box::new((cmd, res.ok()))))
            }
        }
    }

    /// The app quits before the look ended: the download it was handed, the browser's copy
    /// given up already or about to be (an unanswered host takes `timeout` as taken).
    pub fn abandon(mut self) -> mdm_ipc::Command {
        std::mem::take(&mut self.cmd)
    }

    fn say(&mut self, answer: &str) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(answer.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(asked: Instant) -> (Look, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let look = Look {
            cmd: mdm_ipc::Command {
                url: Some("https://files.example/a.tar.gz?token=t".into()),
                browser_handoff: true,
                ..Default::default()
            },
            reply: Some(tx),
            answer: Answer::default(),
            asked,
        };
        (look, rx)
    }

    fn answered(look: &Look, res: Result<Looked, mdm_engine::Error>) {
        *look.answer.lock().unwrap() = Some(res);
    }

    fn not_found() -> mdm_engine::Error {
        mdm_engine::Error::Status(reqwest::StatusCode::NOT_FOUND)
    }

    /// A one-time link the browser spent: the browser keeps its download.
    #[test]
    fn a_spent_address_stays_the_browsers() {
        let (mut look, rx) = look(Instant::now());
        assert!(look.poll().is_none(), "nothing yet");
        assert!(rx.try_recv().is_err(), "the host still waits");
        answered(&look, Err(not_found()));
        assert!(matches!(look.poll(), Some(Settled::Spent)));
        assert_eq!(rx.try_recv().unwrap(), mdm_ipc::SPENT);
    }

    /// The file: taken, with its open answer for the download.
    #[test]
    fn a_file_is_taken_with_its_answer() {
        let (mut look, rx) = look(Instant::now());
        let info = mdm_engine::ProbeInfo {
            final_url: url::Url::parse("https://files.example/a.tar.gz").unwrap(),
            size: Some(27_060),
            file_name: "a.tar.gz".into(),
            server: None,
            web_page: false,
        };
        answered(&look, Ok((info, None)));
        let Some(Settled::Take(taken)) = look.poll() else {
            panic!("taken");
        };
        let (cmd, Some((info, _))) = *taken else {
            panic!("with what it learned");
        };
        assert!(cmd.browser_handoff);
        assert_eq!(info.size, Some(27_060));
        assert_eq!(rx.try_recv().unwrap(), "ok");
    }

    /// A slow server: the host is answered in time, and a refusal after that is the download's
    /// own (the browser let it go).
    #[test]
    fn a_slow_look_is_taken_in_time() {
        let (mut look, rx) = look(Instant::now() - ANSWER_WITHIN);
        assert!(look.poll().is_none());
        assert_eq!(rx.try_recv().unwrap(), "ok");
        answered(&look, Err(not_found()));
        assert!(matches!(look.poll(), Some(Settled::Take(t)) if t.1.is_none()));
    }
}
