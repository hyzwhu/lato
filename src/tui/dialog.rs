//! Async configuration UI. Only the terminal owner reads input and draws output.
use super::{input::InputBuffer, state::AppState, widgets};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::{Stream, StreamExt};
use lato_ai::{AuthInteraction, AuthNotice};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::future::Future;
use tokio::sync::{mpsc, oneshot};

pub const CANCELLED: &str = "dialog cancelled";

#[derive(Clone)]
pub struct Interaction {
    sender: mpsc::UnboundedSender<Request>,
}
enum Request {
    Prompt {
        title: String,
        choices: Vec<String>,
        initial: String,
        secret: bool,
        answer: oneshot::Sender<String>,
    },
    Notice(String),
}
impl Interaction {
    pub async fn input(&self, title: impl Into<String>, secret: bool) -> Result<String, String> {
        self.prompt(title.into(), Vec::new(), String::new(), secret)
            .await
    }
    pub async fn input_with_initial(
        &self,
        title: impl Into<String>,
        initial: impl Into<String>,
        secret: bool,
    ) -> Result<String, String> {
        self.prompt(title.into(), Vec::new(), initial.into(), secret)
            .await
    }
    pub async fn choose(
        &self,
        title: impl Into<String>,
        choices: &[String],
    ) -> Result<String, String> {
        if choices.is_empty() {
            return Err("no choices available".into());
        }
        self.prompt(title.into(), choices.to_vec(), String::new(), false)
            .await
    }
    async fn prompt(
        &self,
        title: String,
        choices: Vec<String>,
        initial: String,
        secret: bool,
    ) -> Result<String, String> {
        let (answer, result) = oneshot::channel();
        self.sender
            .send(Request::Prompt {
                title,
                choices,
                initial,
                secret,
                answer,
            })
            .map_err(|_| CANCELLED.to_string())?;
        result.await.map_err(|_| CANCELLED.to_string())
    }
    pub fn notice(&self, text: impl Into<String>) {
        let _ = self.sender.send(Request::Notice(text.into()));
    }
}
#[async_trait::async_trait]
impl AuthInteraction for Interaction {
    async fn notify(&self, notice: AuthNotice) {
        self.notice(match notice {
            AuthNotice::AuthUrl(url) => {
                format!("Open in browser / 请在浏览器打开:\n{url}\nWaiting for login / 等待登录…")
            }
            AuthNotice::DeviceCode {
                code,
                verification_url,
            } => format!("{verification_url}\nCode / 验证码: {code}"),
            AuthNotice::Info(text) | AuthNotice::Progress(text) => text,
        });
    }
    async fn redirect_url(&self) -> Result<String, String> {
        self.input("Paste callback URL / 粘贴回调链接", true).await
    }
    fn prefers_local_callback(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct Dialog {
    title: String,
    choices: Vec<String>,
    secret: bool,
    input: InputBuffer,
    selected: usize,
    answer: Option<oneshot::Sender<String>>,
    notices: Vec<String>,
}
impl Dialog {
    fn receive(&mut self, request: Request) {
        match request {
            Request::Notice(text) => {
                self.notices.push(text);
            }
            Request::Prompt {
                title,
                choices,
                initial,
                secret,
                answer,
            } => {
                self.title = title;
                self.choices = choices;
                self.secret = secret;
                self.input = InputBuffer::new();
                self.input.insert_str(&initial);
                self.selected = 0;
                self.answer = Some(answer);
                self.notices.clear();
            }
        }
    }
    fn matches(&self) -> Vec<&str> {
        let query = self.input.as_str().to_lowercase();
        let mut matches = self
            .choices
            .iter()
            .filter(|item| item.to_lowercase().contains(&query))
            .map(String::as_str)
            .collect::<Vec<_>>();
        matches.sort_by_key(|item| !item.eq_ignore_ascii_case(&query));
        matches
    }
    fn key(&mut self, key: KeyEvent) {
        if self.answer.is_none() {
            return;
        }
        match key.code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.matches().len().saturating_sub(1))
            }
            KeyCode::Enter => {
                let value = if self.choices.is_empty() {
                    Some(self.input.as_str().to_string())
                } else {
                    self.matches().get(self.selected).map(|s| s.to_string())
                };
                if let Some(value) = value {
                    if let Some(answer) = self.answer.take() {
                        let _ = answer.send(value);
                    }
                    self.input.clear();
                    self.choices.clear();
                    self.secret = false;
                    self.title = "Working / 处理中…".into();
                }
            }
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            KeyCode::Backspace => {
                self.input.backspace();
                self.selected = 0;
            }
            KeyCode::Delete => {
                self.input.delete();
                self.selected = 0;
            }
            KeyCode::Char(c)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.input.insert_char(c);
                self.selected = 0;
            }
            _ => {}
        }
    }
    fn render(&self, frame: &mut Frame<'_>, background: Option<&mut AppState>) {
        if let Some(app) = background {
            super::render::render(frame, app);
        }
        let bounds = frame.area();
        // Cover the composer so its cursor cannot remain visible under a modal.
        let width = bounds.width.saturating_sub(4).min(100);
        let height = bounds.height.saturating_sub(2).min(24);
        let area = Rect::new(
            (bounds.width - width) / 2,
            (bounds.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .title(" Lato · Configuration / 设置 ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(widgets::BLUE))
            .style(Style::default().fg(widgets::TEXT).bg(widgets::RAISED));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new(self.title.as_str()).wrap(Wrap { trim: false }),
            rows[0],
        );
        if self.answer.is_some() {
            let (text, cursor) = self
                .input
                .viewport(rows[1].width.saturating_sub(2) as usize, self.secret);
            frame.render_widget(Paragraph::new(format!("> {text}")), rows[1]);
            if rows[1].width > 2 && rows[1].height > 0 {
                frame.set_cursor_position((rows[1].x + 2 + cursor as u16, rows[1].y));
            }
        }
        if !self.choices.is_empty() {
            let matches = self.matches();
            let items = matches.iter().map(|s| ListItem::new(*s));
            let list = List::new(items).highlight_style(
                Style::default()
                    .fg(widgets::BG)
                    .bg(widgets::AMBER)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(
                list,
                rows[2],
                &mut ListState::default().with_selected(Some(self.selected)),
            );
            if matches.is_empty() {
                frame.render_widget(Paragraph::new("No matches / 无匹配项"), rows[2]);
            }
        } else {
            let lines = self
                .notices
                .iter()
                .flat_map(|s| s.lines().map(|line| Line::raw(line.to_string())))
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[2]);
        }
        frame.render_widget(
            Paragraph::new("↑↓ Select  Enter Confirm  Esc Cancel / 选择·确认·取消")
                .style(Style::default().fg(widgets::MUTED)),
            rows[3],
        );
    }
}

pub async fn run<T, F, Fut, B, S>(
    terminal: &mut ratatui::Terminal<B>,
    events: &mut S,
    mut background: Option<&mut AppState>,
    operation: F,
) -> Result<T, String>
where
    B: ratatui::backend::Backend,
    S: Stream<Item = Result<Event, std::io::Error>> + Unpin,
    F: FnOnce(Interaction) -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let (sender, mut requests) = mpsc::unbounded_channel();
    let ui = Interaction { sender };
    // Keep a sender alive to prevent a closed-channel busy loop before completion.
    let _keepalive = ui.clone();
    let future = operation(ui);
    tokio::pin!(future);
    let mut dialog = Dialog {
        title: "Lato · Working / 处理中…".into(),
        ..Default::default()
    };
    loop {
        terminal
            .draw(|frame| dialog.render(frame, background.as_deref_mut()))
            .map_err(|e| e.to_string())?;
        tokio::select! {
            biased;
            result = &mut future => return result,
            Some(request) = requests.recv() => dialog.receive(request),
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                    if key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                        return Err(CANCELLED.into());
                    }
                    dialog.key(key);
                }
                Some(Ok(Event::Paste(text))) if dialog.answer.is_some() => { dialog.input.insert_str(&text); dialog.selected = 0; }
                Some(Err(e)) => return Err(e.to_string()),
                None => return Err(CANCELLED.into()),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[tokio::test]
    async fn filtered_choice_confirms_selected_value_and_clears_input() {
        let (sender, mut requests) = mpsc::unbounded_channel();
        let ui = Interaction { sender };
        let task = tokio::spawn(async move {
            ui.choose("Models", &["alpha".into(), "beta".into(), "betamax".into()])
                .await
        });
        let mut dialog = Dialog::default();
        dialog.receive(requests.recv().await.unwrap());
        for c in "beta".chars() {
            dialog.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        dialog.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        dialog.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(task.await.unwrap().unwrap(), "betamax");
        assert!(dialog.input.is_empty());
        assert!(dialog.answer.is_none());
    }

    #[test]
    fn secret_input_is_masked_and_cursor_is_on_its_row() {
        let (answer, _) = oneshot::channel();
        let mut dialog = Dialog::default();
        dialog.receive(Request::Prompt {
            title: "API key".into(),
            choices: vec![],
            initial: String::new(),
            secret: true,
            answer,
        });
        dialog.input.insert_str("secret中文");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| dialog.render(frame, None)).unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!text.contains("secret"));
        assert!(!text.contains("中文"));
        assert!(text.contains("••••••••"));
        assert_eq!(buffer[(cursor.x - 1, cursor.y)].symbol(), "•");
        assert_eq!(buffer[(cursor.x, cursor.y)].symbol(), " ");
    }

    #[tokio::test]
    async fn escape_cancels_pending_io_without_waiting_for_network() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut events = futures_util::stream::iter([Ok(Event::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))]);
        let result: Result<(), String> = run(&mut terminal, &mut events, None, |_ui| {
            std::future::pending()
        })
        .await;
        assert_eq!(result, Err(CANCELLED.into()));
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("Still in TUI"), frame.area()))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "S");
    }
}
