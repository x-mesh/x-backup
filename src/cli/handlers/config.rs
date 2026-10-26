//! `config` 서브커맨드 — 기존 config.toml을 방향키 TUI로 편집한다.
//!
//! 화면 흐름: 프로파일 목록 → (새 프로파일 이름) → 필드 편집 → 변경 검토 → 연결 테스트·저장.
//!
//! 필드 목록·출처 판정·변경 조립·저장은 웹 `/config`와 같은 코드를 쓴다
//! ([`crate::web::routes::config`], [`crate::web::config_write`]). 같은 변경이면 웹과 CLI가
//! 같은 파일을 만든다. 저장은 파일 전체를 다시 쓰므로 주석과 키 순서는 남지 않고, 직전본은
//! `<config>.bak.1`에 남는다.
//!
//! 연결 테스트는 저장 전 초안 텍스트로 `status`와 같은 점검을 돌린다. 파일은 쓰지 않는다.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bubbletea_rs::{
    quit, tick, window_size, Cmd, KeyMsg, Model, Msg, PasteMsg, Program, WindowSizeMsg,
};
use bubbletea_widgets::textinput::{self, EchoMode, Model as TextInput};
use crossterm::event::{KeyCode, KeyModifiers};
use lipgloss_extras::lipgloss::{self, AdaptiveColor, Style};

use crate::cli::picker::{ACCENT, MUTED, RULE, SELECTED_BG, SELECTED_FG, TEXT};
use crate::config::file::Config;
use crate::engine::mongo::status::{CheckStatus, StatusReport};
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::web::config_write::FileConfigStore;
use crate::web::job::args::{is_profile_char, ProfileName};
use crate::web::routes::config::{
    blank_profile_view, change_from_values, masked_set_rows, profile_view, validate_field_value,
    ChangeOp, ChangeReceipt, ConfigDocument, ProfileChange, CLEAR_SUFFIX,
};
use crate::web::view::config::{
    FieldGroup, FieldKind, FieldView, Origin, ProfileView, RedactedUri,
};

const WARN: AdaptiveColor = AdaptiveColor {
    Light: "#8A5A00",
    Dark: "#F5C26B",
};
const DANGER: AdaptiveColor = AdaptiveColor {
    Light: "#B42318",
    Dark: "#FF8A80",
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL: Duration = Duration::from_millis(90);
const KEY_COLUMN: usize = 22;
const ORIGIN_COLUMN: usize = 16;

/// `config` 핸들러 진입점.
pub async fn handle(config_path: Option<PathBuf>, lang_flag: Option<Lang>) -> Result<()> {
    let config_toml = config_path
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok());
    let lang = crate::i18n::activate_from_toml(lang_flag, config_toml.as_deref());

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(XBackupError::Usage(crate::tr!(
            "the config editor is interactive (TTY) only — edit config.toml directly in a pipe or CI",
            "config 편집기는 대화형(TTY) 전용입니다 — 파이프/CI에서는 config.toml을 직접 고치세요"
        )));
    }
    let Some(path) = config_path else {
        return Err(XBackupError::Config(crate::tr!(
            "no config.toml found — create one with `x-backup init` first",
            "config.toml을 찾지 못했습니다 — 먼저 `x-backup init`으로 만드세요"
        )));
    };
    let document = ConfigDocument::load(Some(&path))
        .map_err(|error| XBackupError::Config(error.explain(lang)))?;

    let saved = run_editor(EditorSeed {
        path,
        document,
        lang,
    })
    .await?;
    for line in saved {
        println!("{line}");
    }
    Ok(())
}

/// TUI를 실행하고, 종료 후 터미널에 남길 저장 기록을 돌려준다.
async fn run_editor(seed: EditorSeed) -> Result<Vec<String>> {
    let slot = EDITOR_SEED.get_or_init(|| Mutex::new(None));
    *slot.lock().map_err(|_| {
        XBackupError::Failure(crate::tr!(
            "failed to lock the config editor state",
            "config 편집기 상태 잠금 실패"
        ))
    })? = Some(seed);
    let program = Program::<ConfigEditor>::builder()
        .alt_screen(true)
        .bracketed_paste(true)
        .build()
        .map_err(|e| {
            XBackupError::Usage(crate::tr!(
                "failed to start the config editor: {e}",
                "config 편집기 시작 실패: {e}"
            ))
        })?;
    let model = program.run().await.map_err(|e| {
        XBackupError::Usage(crate::tr!(
            "the config editor stopped unexpectedly: {e}",
            "config 편집기가 비정상 종료했습니다: {e}"
        ))
    })?;
    Ok(model.saved_log)
}

struct EditorSeed {
    path: PathBuf,
    document: ConfigDocument,
    lang: Lang,
}

// `Model::init()`은 인자를 받지 않으므로 시작 상태를 정적 슬롯으로 넘긴다(`picker`와 같은 방식).
static EDITOR_SEED: OnceLock<Mutex<Option<EditorSeed>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Profiles,
    NewName,
    Edit,
    Review,
    Saved,
}

/// 필드 하나에 대한 편집. 키가 없으면 그 필드는 건드리지 않은 것이다.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    /// 입력값. 빈 문자열은 직접 적힌 키를 지우거나(직접 출처) 상속을 유지한다.
    Value(String),
    /// 직접 적힌 URI를 지운다(URI는 빈 입력이 "변경 없음"이라 별도 표시가 필요하다).
    ClearUri,
}

enum FieldEditor {
    Text(Box<TextInput>),
    Choice {
        options: Vec<&'static str>,
        index: usize,
    },
}

struct Session {
    op: ChangeOp,
    name: ProfileName,
    view: ProfileView,
    /// 화면에 보이는 필드의 `view.fields` 색인(그룹 순서).
    rows: Vec<usize>,
    cursor: usize,
    edits: BTreeMap<&'static str, Edit>,
    editor: Option<FieldEditor>,
    discard_armed: bool,
}

impl Session {
    fn new(op: ChangeOp, name: ProfileName, view: ProfileView) -> Self {
        let rows = visible_rows(&view);
        Self {
            op,
            name,
            view,
            rows,
            cursor: 0,
            edits: BTreeMap::new(),
            editor: None,
            discard_armed: false,
        }
    }

    fn field(&self) -> Option<&FieldView> {
        self.rows
            .get(self.cursor)
            .and_then(|index| self.view.fields.get(*index))
    }

    fn form_values(&self) -> Vec<(String, String)> {
        let mut values = Vec::new();
        for (key, edit) in &self.edits {
            match edit {
                Edit::Value(raw) => values.push(((*key).to_string(), raw.clone())),
                Edit::ClearUri => {
                    values.push(((*key).to_string(), String::new()));
                    values.push((format!("{key}{CLEAR_SUFFIX}"), "1".to_string()));
                }
            }
        }
        values
    }
}

/// destination을 편집할 수 없는 프로파일에서는 그 그룹을 숨긴다(저장도 그 키를 버린다).
fn visible_rows(view: &ProfileView) -> Vec<usize> {
    let mut rows = Vec::new();
    for group in FieldGroup::ORDER {
        if group == FieldGroup::Destination && !view.destination_editable {
            continue;
        }
        rows.extend(
            view.fields
                .iter()
                .enumerate()
                .filter(|(_, field)| field.group == group)
                .map(|(index, _)| index),
        );
    }
    rows
}

enum TestState {
    Idle,
    Running,
    Done(StatusReport),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Info,
    Warn,
    Danger,
}

struct Notice {
    tone: Tone,
    text: String,
}

struct SavedInfo {
    name: String,
    nothing_changed: bool,
    warnings: Vec<String>,
}

struct TestFinished(std::result::Result<StatusReport, String>);
struct SaveFinished(std::result::Result<ChangeReceipt, String>);
struct SpinnerTick;

struct ConfigEditor {
    lang: Lang,
    path: PathBuf,
    document: ConfigDocument,
    names: Vec<String>,
    views: Vec<Option<ProfileView>>,
    screen: Screen,
    width: u16,
    height: u16,
    list_cursor: usize,
    name_input: TextInput,
    session: Option<Session>,
    change: Option<ProfileChange>,
    test: TestState,
    saving: bool,
    spinner: usize,
    notice: Option<Notice>,
    saved: Option<SavedInfo>,
    saved_log: Vec<String>,
    shared_dest_warning: Option<String>,
}

impl ConfigEditor {
    fn from_seed(seed: EditorSeed) -> Self {
        let mut name_input = textinput::new();
        name_input.prompt = "› ".to_string();
        name_input.prompt_style = Style::new().foreground(ACCENT).bold(true);
        name_input.text_style = Style::new().foreground(TEXT);
        name_input.placeholder_style = Style::new().foreground(MUTED);
        name_input.set_placeholder(seed.lang.sel("type a name", "이름 입력"));
        let mut editor = Self {
            lang: seed.lang,
            path: seed.path,
            document: seed.document,
            names: Vec::new(),
            views: Vec::new(),
            screen: Screen::Profiles,
            width: 100,
            height: 30,
            list_cursor: 0,
            name_input,
            session: None,
            change: None,
            test: TestState::Idle,
            saving: false,
            spinner: 0,
            notice: None,
            saved: None,
            saved_log: Vec::new(),
            shared_dest_warning: None,
        };
        editor.refresh_profiles();
        editor
    }

    fn refresh_profiles(&mut self) {
        self.names = self.document.config.profiles.keys().cloned().collect();
        self.views = self
            .names
            .iter()
            .map(|name| profile_view(self.lang, &self.document, name))
            .collect();
        self.list_cursor = self.list_cursor.min(self.names.len());
    }

    fn info(&mut self, tone: Tone, text: impl Into<String>) {
        self.notice = Some(Notice {
            tone,
            text: text.into(),
        });
    }

    fn handle_key(&mut self, key: KeyMsg) -> Option<Cmd> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.key == KeyCode::Char('c') {
            return Some(quit());
        }
        if self.saving || matches!(self.test, TestState::Running) {
            return None;
        }
        self.notice = None;
        match self.screen {
            Screen::Profiles => self.key_profiles(key),
            Screen::NewName => self.key_new_name(key),
            Screen::Edit => self.key_edit(key),
            Screen::Review => self.key_review(key),
            Screen::Saved => self.key_saved(key),
        }
    }

    // ── 프로파일 목록 ──

    fn key_profiles(&mut self, key: KeyMsg) -> Option<Cmd> {
        let last = self.names.len();
        match key.key {
            KeyCode::Up | KeyCode::Char('k') => {
                self.list_cursor = self.list_cursor.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.list_cursor = (self.list_cursor + 1).min(last)
            }
            KeyCode::Home => self.list_cursor = 0,
            KeyCode::End => self.list_cursor = last,
            KeyCode::Char('n') => return self.open_new_name(),
            KeyCode::Enter | KeyCode::Right => {
                if self.list_cursor == last {
                    return self.open_new_name();
                }
                self.open_profile(self.list_cursor);
            }
            KeyCode::Esc | KeyCode::Char('q') => return Some(quit()),
            _ => {}
        }
        None
    }

    fn open_new_name(&mut self) -> Option<Cmd> {
        self.name_input.set_value("");
        std::mem::drop(self.name_input.focus());
        self.screen = Screen::NewName;
        None
    }

    fn open_profile(&mut self, index: usize) {
        let Some(view) = self.views.get(index).cloned().flatten() else {
            return;
        };
        if !view.editable {
            let reason = view.name_error.clone().unwrap_or_default();
            self.info(Tone::Warn, reason);
            return;
        }
        match ProfileName::parse(&view.name, self.lang) {
            Ok(name) => {
                self.session = Some(Session::new(ChangeOp::Update, name, view));
                self.screen = Screen::Edit;
            }
            Err(error) => self.info(Tone::Warn, error.to_string()),
        }
    }

    // ── 새 프로파일 이름 ──

    fn key_new_name(&mut self, key: KeyMsg) -> Option<Cmd> {
        match key.key {
            KeyCode::Esc => {
                self.name_input.blur();
                self.screen = Screen::Profiles;
                None
            }
            KeyCode::Enter => {
                let raw = self.name_input.value();
                let name = match ProfileName::parse(&raw, self.lang) {
                    Ok(name) => name,
                    Err(error) => {
                        self.info(Tone::Danger, error.to_string());
                        return None;
                    }
                };
                if self.document.config.profiles.contains_key(name.as_str()) {
                    self.info(
                        Tone::Danger,
                        self.lang.sel(
                            "A profile with that name already exists.",
                            "같은 이름의 프로파일이 이미 있습니다.",
                        ),
                    );
                    return None;
                }
                self.name_input.blur();
                let mut view = blank_profile_view(self.lang);
                view.name = name.as_str().to_string();
                self.session = Some(Session::new(ChangeOp::Create, name, view));
                self.screen = Screen::Edit;
                None
            }
            KeyCode::Char(c) if !is_profile_char(c) => {
                self.refuse_name_chars(&c.to_string());
                None
            }
            _ => self.name_input.update(Box::new(key)),
        }
    }

    /// 이름에 쓸 수 없는 글자를 입력 단계에서 막고 이유를 알린다. Enter 때 거부하면 한글
    /// 이름을 끝까지 쳐 놓고서야 다시 지워야 한다.
    fn refuse_name_chars(&mut self, dropped: &str) {
        let shown = dropped.replace(' ', "␠");
        let text = self.lang.sel_string(
            format!(
                "'{shown}' can't be used. Profile names become file paths and the --profile argument, so use English letters, digits, '_' and '-'."
            ),
            format!(
                "'{shown}'은(는) 쓸 수 없습니다. 프로파일 이름은 파일 경로와 --profile 인자에 그대로 쓰이므로 영문·숫자·`_`·`-`로 입력하세요."
            ),
        );
        self.info(Tone::Warn, text);
    }

    // ── 필드 편집 ──

    fn key_edit(&mut self, key: KeyMsg) -> Option<Cmd> {
        let editing = self
            .session
            .as_ref()
            .is_some_and(|session| session.editor.is_some());
        if editing {
            return self.key_field_editor(key);
        }
        let lang = self.lang;
        let session = self.session.as_mut()?;
        if !matches!(key.key, KeyCode::Esc | KeyCode::Char('q')) {
            session.discard_armed = false;
        }
        let last = session.rows.len().saturating_sub(1);
        let page = usize::from(self.height.saturating_sub(12).max(4));
        match key.key {
            KeyCode::Up | KeyCode::Char('k') => session.cursor = session.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => session.cursor = (session.cursor + 1).min(last),
            KeyCode::PageUp => session.cursor = session.cursor.saturating_sub(page),
            KeyCode::PageDown => session.cursor = (session.cursor + page).min(last),
            KeyCode::Home => session.cursor = 0,
            KeyCode::End => session.cursor = last,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.start_field_edit(),
            KeyCode::Char('d') => self.reset_field(),
            KeyCode::Char('u') => {
                if let Some(key) = session.field().map(|field| field.key) {
                    session.edits.remove(key);
                }
            }
            KeyCode::Char('s') | KeyCode::Tab => self.open_review(),
            KeyCode::Char('t') => {
                self.open_review();
                if self.screen == Screen::Review {
                    return self.start_test();
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                if session.edits.is_empty() || session.discard_armed {
                    self.session = None;
                    self.screen = Screen::Profiles;
                } else {
                    session.discard_armed = true;
                    let count = session.edits.len();
                    self.info(
                        Tone::Warn,
                        lang.sel_string(
                            format!("{count} unsaved change(s). Press esc again to discard them."),
                            format!("저장하지 않은 변경이 {count}개 있습니다. esc를 한 번 더 누르면 버립니다."),
                        ),
                    );
                }
            }
            _ => {}
        }
        None
    }

    fn start_field_edit(&mut self) {
        let lang = self.lang;
        let width = self.value_width();
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(field) = session.field().cloned() else {
            return;
        };
        // 편집 중인 값이 있으면 그것을, 없으면 지금 적용 중인 값을 채워 그 자리에서 고치게 한다.
        let start = match session.edits.get(field.key) {
            Some(Edit::Value(raw)) => raw.clone(),
            Some(Edit::ClearUri) => String::new(),
            None => self.current_text(session, &field).unwrap_or_default(),
        };
        let editor = match field.kind {
            FieldKind::Bool | FieldKind::Choice(_) => {
                let mut options = vec![""];
                match field.kind {
                    FieldKind::Bool => options.extend(["true", "false"]),
                    FieldKind::Choice(choices) => options.extend(choices.iter().copied()),
                    _ => {}
                }
                let index = options
                    .iter()
                    .position(|option| *option == start)
                    .unwrap_or(0);
                FieldEditor::Choice { options, index }
            }
            _ => {
                let mut input = textinput::new();
                input.prompt = String::new();
                input.text_style = Style::new().foreground(TEXT);
                input.placeholder_style = Style::new().foreground(MUTED);
                input.set_width(width as i32);
                // 칸을 모두 비웠을 때의 뜻(키 삭제·상속 유지)을 자리표시 문구로 보여 준다.
                // URI도 입력 중에는 가리지 않는다(오타를 찾을 수 있어야 한다). 확정한 뒤
                // 목록·검토 화면에서는 자격증명을 가린다.
                input.set_placeholder(field.origin.empty_option_label(lang));
                input.set_value(&start);
                input.cursor_end();
                std::mem::drop(input.focus());
                FieldEditor::Text(Box::new(input))
            }
        };
        if let Some(session) = self.session.as_mut() {
            session.editor = Some(editor);
        }
    }

    /// 지금 적용 중인 값의 편집용 문자열. URI는 화면 표시용으로 가린 값이 아니라 config에 적힌
    /// 원문이다 — 가린 값을 채우면 그대로 저장했을 때 접속 정보가 망가진다.
    fn current_text(&self, session: &Session, field: &FieldView) -> Option<String> {
        if field.kind != FieldKind::Uri {
            return field.effective.clone();
        }
        let source = &self
            .document
            .config
            .profiles
            .get(session.name.as_str())?
            .source;
        match field.key {
            "uri" => source.uri.clone(),
            "read_uri" => source.read_uri.clone(),
            _ => None,
        }
    }

    fn key_field_editor(&mut self, key: KeyMsg) -> Option<Cmd> {
        let session = self.session.as_mut()?;
        let editor = session.editor.as_mut()?;
        match editor {
            FieldEditor::Choice { options, index } => match key.key {
                KeyCode::Left | KeyCode::Char('h') => {
                    *index = index.checked_sub(1).unwrap_or(options.len() - 1);
                }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                    *index = (*index + 1) % options.len();
                }
                KeyCode::Enter => {
                    let raw = options[*index].to_string();
                    self.commit_field(raw);
                }
                KeyCode::Esc => session.editor = None,
                _ => {}
            },
            FieldEditor::Text(input) => match key.key {
                KeyCode::Enter => {
                    let raw = input.value().trim().to_string();
                    self.commit_field(raw);
                }
                KeyCode::Esc => session.editor = None,
                _ => return input.update(Box::new(key)),
            },
        }
        None
    }

    /// 입력을 확정한다. 웹 폼의 빈 값 규약(`build_change`)을 그대로 따른다.
    fn commit_field(&mut self, raw: String) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(field) = session.field().cloned() else {
            return;
        };
        let unchanged = raw == self.current_text(session, &field).unwrap_or_default();
        if !unchanged && !raw.is_empty() {
            if let Err(error) = validate_field_value(field.key, &raw) {
                self.info(Tone::Danger, error.to_string());
                return;
            }
        }
        let carries_secret = !unchanged
            && field.kind == FieldKind::Uri
            && RedactedUri::from_raw(&raw).shown() != raw;
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if unchanged || (raw.is_empty() && !field.origin.is_direct()) {
            // 그대로 두었거나, 상속값을 비웠다 — 둘 다 "건드리지 않음"이다.
            session.edits.remove(field.key);
        } else if raw.is_empty() {
            // 직접 적힌 값을 비웠다 — 그 키를 지운다. URI는 빈 값이 "변경 없음"이라 지우기
            // 표시를 따로 보낸다.
            let edit = if field.kind == FieldKind::Uri {
                Edit::ClearUri
            } else {
                Edit::Value(String::new())
            };
            session.edits.insert(field.key, edit);
        } else {
            session.edits.insert(field.key, Edit::Value(raw));
        }
        session.editor = None;
        if field.group == FieldGroup::Destination {
            self.warn_if_destination_is_shared();
        }
        if carries_secret {
            let path = self.path.display().to_string();
            let text = self.lang.sel_string(
                format!("The password is stored in plain text in the config file and its .bak copies. Keep them private: chmod 600 {path}*"),
                format!("비밀번호가 config 파일과 직전본(.bak)에 평문으로 저장됩니다. 다른 사용자가 읽지 못하게 하세요: chmod 600 {path}*"),
            );
            self.info(Tone::Warn, text);
        }
    }

    /// `d` — 직접 적힌 값을 지운다. 상속 중인 필드는 지울 것이 없다.
    fn reset_field(&mut self) {
        let lang = self.lang;
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let Some(field) = session.field().cloned() else {
            return;
        };
        if !field.origin.is_direct() {
            self.info(
                Tone::Info,
                lang.sel(
                    "This value is not written in the profile, so there is nothing to remove.",
                    "이 값은 프로파일에 직접 적혀 있지 않아 지울 것이 없습니다.",
                ),
            );
            return;
        }
        let edit = if field.kind == FieldKind::Uri {
            Edit::ClearUri
        } else {
            Edit::Value(String::new())
        };
        session.edits.insert(field.key, edit);
    }

    // ── 검토·테스트·저장 ──

    fn open_review(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        match change_from_values(
            self.lang,
            session.op,
            session.name.clone(),
            &self.document,
            session.form_values(),
        ) {
            Ok(change) => {
                let shared = self.draft_shared_destinations(&change);
                self.shared_dest_warning = Config::shared_destination_warning(&shared, self.lang);
                self.change = Some(change);
                self.test = TestState::Idle;
                self.screen = Screen::Review;
            }
            Err(error) => self.info(Tone::Danger, error.to_string()),
        }
    }

    /// 초안을 적용했을 때 이 프로파일과 백업 위치가 같은 다른 프로파일들. 초안을 만들 수 없으면
    /// (예: 소스가 아직 없는 새 프로파일) 비어 있다 — 그때는 저장 전 `doctor`가 다시 본다.
    fn draft_shared_destinations(&self, change: &ProfileChange) -> Vec<String> {
        let Ok(draft) = FileConfigStore::new(self.path.clone()).render(change) else {
            return Vec::new();
        };
        let Ok(config) = Config::from_toml_str(&draft) else {
            return Vec::new();
        };
        config.profiles_sharing_destination(change.name.as_str())
    }

    /// destination 필드를 확정한 직후, 다른 프로파일과 위치가 겹치면 그 자리에서 알린다.
    fn warn_if_destination_is_shared(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Ok(change) = change_from_values(
            self.lang,
            session.op,
            session.name.clone(),
            &self.document,
            session.form_values(),
        ) else {
            return;
        };
        let shared = self.draft_shared_destinations(&change);
        if let Some(text) = Config::shared_destination_warning(&shared, self.lang) {
            self.info(Tone::Warn, text);
        }
    }

    fn key_review(&mut self, key: KeyMsg) -> Option<Cmd> {
        match key.key {
            KeyCode::Char('t') => self.start_test(),
            KeyCode::Char('s') | KeyCode::Enter => self.start_save(),
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('e') => {
                self.screen = Screen::Edit;
                None
            }
            _ => None,
        }
    }

    fn start_test(&mut self) -> Option<Cmd> {
        let change = self.change.as_ref()?;
        let draft = match FileConfigStore::new(self.path.clone()).render(change) {
            Ok(text) => text,
            Err(error) => {
                self.test = TestState::Failed(error.to_string());
                return None;
            }
        };
        let name = change.name.as_str().to_string();
        let lang = self.lang;
        self.test = TestState::Running;
        let run: Cmd = Box::pin(async move {
            let report =
                crate::cli::handlers::status::build_report(Some(draft.as_str()), &name, lang)
                    .await
                    .map_err(|error| error.to_string());
            Some(Box::new(TestFinished(report)) as Msg)
        });
        Some(bubbletea_rs::batch(vec![run, spinner_tick()]))
    }

    fn start_save(&mut self) -> Option<Cmd> {
        let change = self.change.clone()?;
        if change.is_noop() && change.op == ChangeOp::Update {
            self.info(
                Tone::Info,
                self.lang.sel("Nothing to save.", "저장할 변경이 없습니다."),
            );
            return None;
        }
        let path = self.path.clone();
        self.saving = true;
        let run: Cmd = Box::pin(async move {
            // doctor 자식 대기·파일 교체가 동기 I/O라 이벤트 루프를 막지 않게 블로킹 풀로 보낸다.
            let result = tokio::task::spawn_blocking(move || {
                FileConfigStore::new(path).apply_from_terminal(&change)
            })
            .await;
            let outcome = match result {
                Ok(Ok(receipt)) => Ok(receipt),
                Ok(Err(error)) => Err(error.to_string()),
                Err(join) => Err(join.to_string()),
            };
            Some(Box::new(SaveFinished(outcome)) as Msg)
        });
        Some(bubbletea_rs::batch(vec![run, spinner_tick()]))
    }

    fn finish_save(&mut self, outcome: std::result::Result<ChangeReceipt, String>) {
        self.saving = false;
        let receipt = match outcome {
            Ok(receipt) => receipt,
            Err(error) => {
                self.info(Tone::Danger, error);
                return;
            }
        };
        let Some(change) = self.change.take() else {
            return;
        };
        let warnings = match receipt {
            ChangeReceipt::PersistedWithWarnings(warnings) => warnings,
            _ => Vec::new(),
        };
        let name = change.name.as_str().to_string();
        self.saved_log.push(self.lang.sel_string(
            format!(
                "saved profile '{name}' to {} (previous version: {})",
                self.path.display(),
                backup_path(&self.path).display()
            ),
            format!(
                "프로파일 '{name}'을 {}에 저장했습니다 (직전본: {})",
                self.path.display(),
                backup_path(&self.path).display()
            ),
        ));
        self.saved = Some(SavedInfo {
            name: name.clone(),
            nothing_changed: change.is_noop(),
            warnings,
        });
        self.session = None;
        match ConfigDocument::load(Some(&self.path)) {
            Ok(document) => {
                self.document = document;
                self.refresh_profiles();
                if let Some(index) = self.names.iter().position(|candidate| *candidate == name) {
                    self.list_cursor = index;
                }
            }
            Err(error) => self.info(Tone::Danger, error.explain(self.lang)),
        }
        self.screen = Screen::Saved;
    }

    fn key_saved(&mut self, key: KeyMsg) -> Option<Cmd> {
        match key.key {
            KeyCode::Esc | KeyCode::Char('q') => Some(quit()),
            _ => {
                self.saved = None;
                self.screen = Screen::Profiles;
                None
            }
        }
    }

    fn value_width(&self) -> usize {
        usize::from(self.width)
            .saturating_sub(KEY_COLUMN + ORIGIN_COLUMN + 14)
            .max(16)
    }
}

fn spinner_tick() -> Cmd {
    tick(SPINNER_INTERVAL, |_| Box::new(SpinnerTick) as Msg)
}

fn backup_name(path: &Path) -> String {
    backup_path(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".bak.1");
    PathBuf::from(name)
}

impl Model for ConfigEditor {
    fn init() -> (Self, Option<Cmd>) {
        let seed = EDITOR_SEED
            .get_or_init(|| Mutex::new(None))
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        // `run_editor`가 항상 먼저 채운다. 비어 있다면 빈 목록으로 열어 종료만 가능하게 한다.
        let seed = seed.unwrap_or_else(|| EditorSeed {
            path: PathBuf::new(),
            document: ConfigDocument {
                syntax: crate::web::view::config::ConfigSyntax::V2,
                config: crate::config::file::Config::default(),
                origins: BTreeMap::new(),
            },
            lang: Lang::En,
        });
        (Self::from_seed(seed), Some(window_size()))
    }

    fn update(&mut self, msg: Msg) -> Option<Cmd> {
        if let Some(size) = msg.downcast_ref::<WindowSizeMsg>() {
            self.width = size.width.max(60);
            self.height = size.height.max(20);
            return None;
        }
        if msg.downcast_ref::<SpinnerTick>().is_some() {
            if self.saving || matches!(self.test, TestState::Running) {
                self.spinner = (self.spinner + 1) % SPINNER.len();
                return Some(spinner_tick());
            }
            return None;
        }
        if let Some(done) = msg.downcast_ref::<TestFinished>() {
            self.test = match &done.0 {
                Ok(report) => TestState::Done(report.clone()),
                Err(error) => TestState::Failed(error.clone()),
            };
            return None;
        }
        if let Some(done) = msg.downcast_ref::<SaveFinished>() {
            self.finish_save(done.0.clone());
            return None;
        }
        if let Some(paste) = msg.downcast_ref::<PasteMsg>() {
            return self.paste(paste.clone());
        }
        let key = msg.downcast_ref::<KeyMsg>().cloned()?;
        self.handle_key(key)
    }

    fn view(&self) -> String {
        let body = match self.screen {
            Screen::Profiles => self.view_profiles(),
            Screen::NewName => self.view_new_name(),
            Screen::Edit => self.view_edit(),
            Screen::Review => self.view_review(),
            Screen::Saved => self.view_saved(),
        };
        Style::new()
            .width(i32::from(self.width.saturating_sub(2)))
            .padding(1, 1, 0, 1)
            .render(&body)
    }
}

impl ConfigEditor {
    fn paste(&mut self, paste: PasteMsg) -> Option<Cmd> {
        match self.screen {
            Screen::NewName => {
                let (kept, dropped): (String, String) =
                    paste.0.chars().partition(|c| is_profile_char(*c));
                if !dropped.is_empty() {
                    self.refuse_name_chars(&dropped);
                }
                if kept.is_empty() {
                    None
                } else {
                    self.name_input.update(Box::new(textinput::PasteMsg(kept)))
                }
            }
            Screen::Edit => match self.session.as_mut()?.editor.as_mut()? {
                FieldEditor::Text(input) => input.update(Box::new(textinput::PasteMsg(paste.0))),
                FieldEditor::Choice { .. } => None,
            },
            _ => None,
        }
    }

    // ── 렌더 ──

    fn header(&self, trail: &[&str]) -> String {
        let mut title = Style::new()
            .bold(true)
            .foreground(TEXT)
            .render("x-backup config");
        for part in trail {
            title.push_str(&Style::new().foreground(MUTED).render("  ›  "));
            title.push_str(&Style::new().bold(true).foreground(TEXT).render(part));
        }
        let syntax = self.document.syntax.label();
        let room =
            usize::from(self.width.saturating_sub(8)).saturating_sub(lipgloss::width(syntax));
        let meta = Style::new().foreground(MUTED).render(&format!(
            "{} · {syntax}",
            fit_tail(&self.path.display().to_string(), room)
        ));
        format!("{title}\n{meta}\n{}", self.rule())
    }

    fn rule(&self) -> String {
        Style::new()
            .foreground(RULE)
            .render(&"─".repeat(usize::from(self.width.saturating_sub(4)).max(1)))
    }

    fn notice_line(&self) -> String {
        let Some(notice) = &self.notice else {
            return String::new();
        };
        let color = match notice.tone {
            Tone::Info => MUTED,
            Tone::Warn => WARN,
            Tone::Danger => DANGER,
        };
        format!("\n{}", Style::new().foreground(color).render(&notice.text))
    }

    fn help(&self, text: &str) -> String {
        format!("\n{}", Style::new().foreground(MUTED).render(text))
    }

    fn view_profiles(&self) -> String {
        let lang = self.lang;
        let mut out = self.header(&[]);
        out.push('\n');
        let default = self.document.config.default_profile.as_deref();
        let width = usize::from(self.width.saturating_sub(4));
        for (index, name) in self.names.iter().enumerate() {
            let selected = index == self.list_cursor;
            let view = self.views.get(index).and_then(Option::as_ref);
            let mark = if default == Some(name.as_str()) {
                "★"
            } else {
                " "
            };
            let source = view.map(source_summary).unwrap_or_default();
            let dest = view
                .and_then(|view| view.field("dest"))
                .and_then(|field| field.effective.clone())
                .unwrap_or_else(|| "—".to_string());
            let line = format!(
                "{} {mark} {}  {}  {}",
                if selected { "›" } else { " " },
                fit(name, 16),
                fit(&source, 28),
                fit(&dest, width.saturating_sub(52).max(10))
            );
            out.push('\n');
            out.push_str(
                &row_style(selected, view.is_some_and(|view| view.editable)).render(&line),
            );
        }
        let add = format!(
            "{}   ＋ {}",
            if self.list_cursor == self.names.len() {
                "›"
            } else {
                " "
            },
            lang.sel("add a new profile", "새 프로파일 추가")
        );
        out.push('\n');
        out.push_str(&row_style(self.list_cursor == self.names.len(), true).render(&add));
        out.push_str(&format!("\n\n{}", self.rule()));
        out.push_str(&self.profile_detail());
        out.push_str(&self.notice_line());
        out.push_str(&self.help(lang.sel(
            "↑↓ move  enter edit  n new profile  q quit   ★ default profile",
            "↑↓ 이동  enter 편집  n 새 프로파일  q 종료   ★ 기본 프로파일",
        )));
        out
    }

    fn profile_detail(&self) -> String {
        let lang = self.lang;
        let Some(view) = self.views.get(self.list_cursor).and_then(Option::as_ref) else {
            return format!(
                "\n{}",
                Style::new().foreground(MUTED).render(lang.sel(
                    "Create a profile with only the fields you need; the rest inherits defaults.",
                    "필요한 필드만 채워 프로파일을 만들고, 나머지는 기본값을 따릅니다.",
                ))
            );
        };
        let counts = lang.sel_string(
            format!(
                "{} fields written here, {} inherited",
                view.direct_count(),
                view.inherited_count()
            ),
            format!(
                "직접 적은 필드 {}개, 상속받은 필드 {}개",
                view.direct_count(),
                view.inherited_count()
            ),
        );
        let missing: Vec<&str> = view
            .secrets
            .iter()
            .filter(|secret| !secret.is_present())
            .map(|secret| secret.name())
            .collect();
        let mut out = format!("\n{}", Style::new().foreground(MUTED).render(&counts));
        if !missing.is_empty() {
            out.push_str(&format!(
                "\n{}",
                Style::new().foreground(WARN).render(&lang.sel_string(
                    format!(
                        "not set in this shell: {} — the connection test will fail",
                        missing.join(", ")
                    ),
                    format!(
                        "현재 셸에 없는 환경변수: {} — 연결 테스트가 실패합니다",
                        missing.join(", ")
                    ),
                ))
            ));
        }
        if let Some(reason) = view.name_error.as_deref().filter(|_| !view.editable) {
            out.push_str(&format!(
                "\n{}",
                Style::new().foreground(WARN).render(reason)
            ));
        }
        out
    }

    fn view_new_name(&self) -> String {
        let lang = self.lang;
        let mut out = self.header(&[lang.sel("new profile", "새 프로파일")]);
        out.push_str(&format!(
            "\n\n{}\n{}\n\n{}",
            Style::new()
                .bold(true)
                .foreground(TEXT)
                .render(lang.sel("Profile name", "프로파일 이름")),
            render_input(&self.name_input, 40),
            Style::new().foreground(MUTED).render(lang.sel(
                "English letters, digits, '_' and '-' only, starting with a letter or digit. Korean is not allowed. e.g. prod, staging, kr-main",
                "영문·숫자·`_`·`-`만 쓰고 영문이나 숫자로 시작합니다. 한글은 쓸 수 없습니다. 예: prod, staging, kr-main",
            ))
        ));
        out.push_str(&self.notice_line());
        out.push_str(&self.help(lang.sel("enter next  esc cancel", "enter 다음  esc 취소")));
        out
    }

    fn view_edit(&self) -> String {
        let lang = self.lang;
        let Some(session) = self.session.as_ref() else {
            return String::new();
        };
        let mode = match session.op {
            ChangeOp::Create => lang.sel("new", "새 프로파일"),
            _ => lang.sel("edit", "편집"),
        };
        let mut out = self.header(&[session.name.as_str(), mode]);

        let mut lines: Vec<(Option<usize>, String)> = Vec::new();
        let mut group = None;
        for (row, index) in session.rows.iter().enumerate() {
            let field = &session.view.fields[*index];
            if group != Some(field.group) {
                group = Some(field.group);
                lines.push((
                    None,
                    Style::new()
                        .bold(true)
                        .foreground(ACCENT)
                        .render(field.group.label()),
                ));
            }
            lines.push((Some(row), self.field_line(session, row, field)));
        }
        let capacity = usize::from(self.height.saturating_sub(13)).max(5);
        let selected_line = lines
            .iter()
            .position(|(row, _)| *row == Some(session.cursor))
            .unwrap_or(0);
        let start = selected_line
            .saturating_sub(capacity.saturating_sub(2))
            .min(lines.len().saturating_sub(capacity));
        out.push('\n');
        for (_, line) in lines.iter().skip(start).take(capacity) {
            out.push('\n');
            out.push_str(line);
        }
        if let Some(note) = session
            .view
            .destination_note
            .as_deref()
            .filter(|_| !session.view.destination_editable)
        {
            out.push_str(&format!(
                "\n{}",
                Style::new().foreground(MUTED).render(note)
            ));
        }
        out.push_str(&format!("\n\n{}", self.rule()));
        out.push_str(&self.field_detail(session));
        out.push_str(&self.notice_line());
        let help = match &session.editor {
            Some(FieldEditor::Choice { .. }) => lang.sel(
                "←→ choose  enter confirm  esc cancel",
                "←→ 선택  enter 확정  esc 취소",
            ),
            Some(FieldEditor::Text(_)) => lang.sel(
                "type value  enter confirm  esc cancel",
                "값 입력  enter 확정  esc 취소",
            ),
            None => lang.sel(
                "↑↓ move  enter edit  d remove key  u undo  t test  s review & save  esc back",
                "↑↓ 이동  enter 편집  d 키 삭제  u 되돌리기  t 연결 테스트  s 검토·저장  esc 목록",
            ),
        };
        let count = session.edits.len();
        let pending = if count > 0 {
            Style::new().bold(true).foreground(ACCENT).render(
                &lang.sel_string(format!("{count} change(s)  "), format!("변경 {count}개  ")),
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "\n{pending}{}",
            Style::new().foreground(MUTED).render(help)
        ));
        out
    }

    fn field_line(&self, session: &Session, row: usize, field: &FieldView) -> String {
        let lang = self.lang;
        let selected = row == session.cursor;
        let edit = session.edits.get(field.key);
        let marker = if selected { "›" } else { " " };
        let dot = if edit.is_some() {
            Style::new().foreground(ACCENT).render("●")
        } else {
            " ".to_string()
        };
        let key = Style::new()
            .foreground(TEXT)
            .bold(selected)
            .render(&fit(field.key, KEY_COLUMN));
        let width = self.value_width();
        let value = match (&session.editor, selected) {
            (Some(FieldEditor::Text(input)), true) => render_input(input, width),
            (Some(FieldEditor::Choice { options, index }), true) => {
                let shown = if options[*index].is_empty() {
                    field.origin.empty_option_label(lang)
                } else {
                    options[*index]
                };
                Style::new()
                    .foreground(SELECTED_FG)
                    .background(SELECTED_BG)
                    .render(&format!("‹ {shown} ›"))
            }
            _ => match edit {
                Some(Edit::Value(raw)) if raw.is_empty() => Style::new()
                    .foreground(WARN)
                    .render(field.origin.empty_option_label(lang)),
                Some(Edit::Value(raw)) => Style::new()
                    .foreground(ACCENT)
                    .render(&fit(&shown_value(field.kind, raw), width)),
                Some(Edit::ClearUri) => Style::new()
                    .foreground(WARN)
                    .render(lang.sel("(remove this key)", "(이 키 삭제)")),
                None => match field.effective.as_deref() {
                    Some(value) => Style::new().foreground(TEXT).render(&fit(value, width)),
                    None => Style::new().foreground(MUTED).render("—"),
                },
            },
        };
        let origin = if edit.is_some() {
            Style::new()
                .foreground(ACCENT)
                .render(lang.sel("changed", "변경됨"))
        } else {
            Style::new()
                .foreground(MUTED)
                .render(fit(&field.origin.label(), ORIGIN_COLUMN).trim_end())
        };
        let padded = pad_to(&value, width);
        let line = format!("{marker} {dot} {key} {padded}  {origin}");
        if selected && session.editor.is_none() {
            Style::new().background(SELECTED_BG).render(&line)
        } else {
            line
        }
    }

    fn field_detail(&self, session: &Session) -> String {
        let lang = self.lang;
        let Some(field) = session.field() else {
            return String::new();
        };
        let title = format!(
            "{}  {}",
            Style::new().bold(true).foreground(TEXT).render(field.key),
            Style::new().foreground(MUTED).render(&format!(
                "{}: {}",
                lang.sel("source", "출처"),
                origin_text(&field.origin, lang)
            ))
        );
        let hint = if field.kind == FieldKind::Uri {
            format!(
                "{} {}",
                field.origin.empty_means(lang),
                lang.sel(
                    "Credentials (user:pass@) are stored in plain text in the config file.",
                    "자격증명(`user:pass@`)을 넣으면 config 파일에 평문으로 저장됩니다.",
                )
            )
        } else {
            field.origin.empty_means(lang).to_string()
        };
        let mut out = format!(
            "\n{title}\n{}",
            Style::new().foreground(MUTED).render(&hint)
        );
        if let Some(warning) = &field.warning {
            out.push_str(&format!(
                "\n{}",
                Style::new().foreground(WARN).render(warning)
            ));
        }
        out
    }

    fn view_review(&self) -> String {
        let lang = self.lang;
        let Some(change) = self.change.as_ref() else {
            return String::new();
        };
        let mut out = self.header(&[change.name.as_str(), lang.sel("review", "검토")]);
        out.push('\n');
        if let Some(warning) = &self.shared_dest_warning {
            out.push_str(&format!(
                "\n{}",
                Style::new().foreground(WARN).render(warning)
            ));
        }
        let set = masked_set_rows(change);
        let unset = change.unset_rows();
        if set.is_empty() && unset.is_empty() {
            out.push_str(&format!(
                "\n{}",
                Style::new().foreground(MUTED).render(lang.sel(
                    "No values changed. You can still run the connection test on the current settings.",
                    "바뀐 값이 없습니다. 현재 설정으로 연결 테스트는 할 수 있습니다.",
                ))
            ));
        }
        for (key, value) in &set {
            let inherited = change.broke_inheritance.iter().any(|broken| broken == key);
            let note = if inherited {
                Style::new()
                    .foreground(WARN)
                    .render(lang.sel("  overrides an inherited value", "  상속값을 덮어씀"))
            } else {
                String::new()
            };
            out.push_str(&format!(
                "\n  {}  {} = {}{note}",
                Style::new().bold(true).foreground(ACCENT).render("set  "),
                fit(key, KEY_COLUMN),
                value
            ));
        }
        for key in &unset {
            out.push_str(&format!(
                "\n  {}  {}",
                Style::new().bold(true).foreground(WARN).render("unset"),
                key
            ));
        }
        out.push_str(&format!(
            "\n\n{}\n{}",
            self.rule(),
            Style::new()
                .bold(true)
                .foreground(TEXT)
                .render(lang.sel("Connection test", "연결 테스트"))
        ));
        out.push_str(&self.test_lines());
        out.push_str(&format!(
            "\n\n{}\n{}",
            self.rule(),
            Style::new().foreground(MUTED).render(&lang.sel_string(
                format!(
                    "Saving rewrites the whole file after `doctor` validates it. Comments and key order are not kept; the previous version goes to {}.",
                    backup_name(&self.path)
                ),
                format!(
                    "저장하면 `doctor` 검증 뒤 파일 전체를 다시 씁니다. 주석과 키 순서는 남지 않고, 직전본은 {}에 남습니다.",
                    backup_name(&self.path)
                ),
            ))
        ));
        if self.saving {
            out.push_str(&format!(
                "\n{} {}",
                Style::new()
                    .foreground(ACCENT)
                    .render(SPINNER[self.spinner]),
                lang.sel("validating and saving…", "검증하고 저장하는 중…")
            ));
        }
        out.push_str(&self.notice_line());
        out.push_str(&self.help(lang.sel(
            "t test connection  s save  esc back to editing",
            "t 연결 테스트  s 저장  esc 편집으로 돌아가기",
        )));
        out
    }

    fn test_lines(&self) -> String {
        let lang = self.lang;
        match &self.test {
            TestState::Idle => format!(
                "\n{}",
                Style::new().foreground(MUTED).render(lang.sel(
                    "Not run yet. Press t to check the draft before saving (uses this shell's environment).",
                    "아직 실행하지 않았습니다. t를 누르면 저장 전 초안으로 점검합니다(현재 셸의 환경변수 사용).",
                ))
            ),
            TestState::Running => format!(
                "\n{} {}",
                Style::new().foreground(ACCENT).render(SPINNER[self.spinner]),
                lang.sel("connecting…", "연결하는 중…")
            ),
            TestState::Failed(error) => {
                format!("\n{}", Style::new().foreground(DANGER).render(error))
            }
            TestState::Done(report) => {
                let width = usize::from(self.width).saturating_sub(36).max(20);
                let mut out = String::new();
                for item in &report.items {
                    let (badge, color) = match item.status {
                        CheckStatus::Ok => ("OK  ", ACCENT),
                        CheckStatus::Warn => ("WARN", WARN),
                        CheckStatus::Fail => ("FAIL", DANGER),
                    };
                    out.push_str(&format!(
                        "\n  {}  {}  {}",
                        Style::new().bold(true).foreground(color).render(badge),
                        fit(item.label, 20),
                        fit(item.message.lines().next().unwrap_or_default(), width)
                    ));
                }
                out
            }
        }
    }

    fn view_saved(&self) -> String {
        let lang = self.lang;
        let mut out = self.header(&[lang.sel("saved", "저장됨")]);
        if let Some(saved) = &self.saved {
            let headline = if saved.nothing_changed {
                lang.sel_string(
                    format!(
                        "Profile '{}' already matched; nothing was written.",
                        saved.name
                    ),
                    format!(
                        "프로파일 '{}'은 이미 같은 값이라 쓰지 않았습니다.",
                        saved.name
                    ),
                )
            } else {
                lang.sel_string(
                    format!("Saved profile '{}'.", saved.name),
                    format!("프로파일 '{}'을 저장했습니다.", saved.name),
                )
            };
            out.push_str(&format!(
                "\n\n{}\n{}",
                Style::new().bold(true).foreground(ACCENT).render(&headline),
                Style::new().foreground(MUTED).render(&lang.sel_string(
                    format!(
                        "previous version: {}",
                        fit_tail(
                            &backup_path(&self.path).display().to_string(),
                            usize::from(self.width.saturating_sub(20))
                        )
                    ),
                    format!(
                        "직전본: {}",
                        fit_tail(
                            &backup_path(&self.path).display().to_string(),
                            usize::from(self.width.saturating_sub(20))
                        )
                    ),
                ))
            ));
            for warning in &saved.warnings {
                out.push_str(&format!(
                    "\n{}",
                    Style::new().foreground(WARN).render(warning)
                ));
            }
        }
        out.push_str(&self.notice_line());
        out.push_str(&self.help(lang.sel(
            "enter back to profiles  q quit",
            "enter 프로파일 목록  q 종료",
        )));
        out
    }
}

fn row_style(selected: bool, enabled: bool) -> Style {
    let base = Style::new().foreground(if enabled { TEXT } else { MUTED });
    if selected {
        base.bold(true)
            .foreground(SELECTED_FG)
            .background(SELECTED_BG)
    } else {
        base
    }
}

fn source_summary(view: &ProfileView) -> String {
    if let Some(env) = view
        .field("uri_env")
        .and_then(|field| field.effective.clone())
    {
        return format!("env:{env}");
    }
    view.field("uri")
        .and_then(|field| field.effective.clone())
        .unwrap_or_else(|| "—".to_string())
}

fn origin_text(origin: &Origin, lang: Lang) -> String {
    match origin {
        Origin::Direct => lang
            .sel("written in this profile", "이 프로파일에 직접 적음")
            .to_string(),
        Origin::Defaults => "[defaults]".to_string(),
        Origin::Inherited { from } => format!("extends → {from}"),
        Origin::Builtin => lang.sel("built-in default", "내장 기본값").to_string(),
    }
}

/// 편집한 값의 표시 문자열. URI는 자격증명을 가린다.
fn shown_value(kind: FieldKind, raw: &str) -> String {
    if kind == FieldKind::Uri {
        RedactedUri::from_raw(raw).shown().to_string()
    } else {
        raw.to_string()
    }
}

/// 입력 칸을 그린다.
///
/// bubbletea-widgets 0.1.12의 `textinput::Model::view()`는 글자 위치를 바이트 위치로 써서
/// 문자열을 자르므로, 한글 같은 다중 바이트 글자가 들어오면 panic하고 TUI가 그대로 끝난다.
/// 그래서 값이 있으면 직접 그린다. 빈 칸의 자리표시 문구는 글자 단위로 그려져 안전하다.
fn render_input(input: &TextInput, width: usize) -> String {
    let value: Vec<char> = input.value().chars().collect();
    if value.is_empty() {
        return input.view();
    }
    let shown: Vec<char> = match input.echo_mode {
        EchoMode::EchoPassword => vec![input.echo_character; value.len()],
        EchoMode::EchoNone => Vec::new(),
        EchoMode::EchoNormal => value,
    };
    let cells = |chars: &[char]| chars.iter().map(|c| char_width(*c)).sum::<usize>();
    let pos = input.position().min(shown.len());
    // 커서가 칸 밖으로 나가지 않도록 앞쪽부터 감춘다.
    let mut start = 0;
    while start < pos && cells(&shown[start..pos]) + 1 > width {
        start += 1;
    }
    let before: String = shown[start..pos].iter().collect();
    let at = shown.get(pos).copied().unwrap_or(' ');
    let mut used = cells(&shown[start..pos]) + char_width(at);
    let mut after = String::new();
    for c in shown.iter().skip(pos + 1) {
        let w = char_width(*c);
        if used + w > width {
            break;
        }
        after.push(*c);
        used += w;
    }
    format!(
        "{}{}{}{}",
        input.prompt_style.render(&input.prompt),
        input.text_style.render(&before),
        Style::new().reverse(true).render(&at.to_string()),
        input.text_style.render(&after)
    )
}

fn char_width(c: char) -> usize {
    lipgloss::width(c.encode_utf8(&mut [0u8; 4]))
}

/// 표시 폭(한글 2칸)에 맞춰 자르고 남는 칸은 공백으로 채운다.
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    let total = lipgloss::width(text);
    for ch in text.chars() {
        let w = char_width(ch);
        let reserve = usize::from(total > width);
        if used + w + reserve > width {
            out.push('…');
            used += 1;
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// 표시 폭에 맞춰 **앞쪽**을 잘라 낸다. 경로는 끝부분(파일 이름)이 더 중요하다.
fn fit_tail(text: &str, width: usize) -> String {
    if lipgloss::width(text) <= width {
        return text.to_string();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 1;
    for ch in text.chars().rev() {
        let w = char_width(ch);
        if used + w > width {
            break;
        }
        kept.push(ch);
        used += w;
    }
    kept.reverse();
    format!("…{}", kept.into_iter().collect::<String>())
}

/// 이미 스타일이 입혀진 문자열 뒤를 공백으로 채운다.
fn pad_to(styled: &str, width: usize) -> String {
    let visible = lipgloss::width(styled);
    format!("{styled}{}", " ".repeat(width.saturating_sub(visible)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
default_profile = "prod"

[defaults]
compress = "zstd:3"

[profile.prod]
uri_env = "XB_CONFIG_TUI_TEST_URI"
dest = "local:/tmp/x-backup-tui"
keep_full = 4
"#;

    fn editor(text: &str) -> (tempfile::TempDir, ConfigEditor) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        let document = ConfigDocument::load(Some(&path)).unwrap();
        let editor = ConfigEditor::from_seed(EditorSeed {
            path,
            document,
            lang: Lang::En,
        });
        (dir, editor)
    }

    fn key(code: KeyCode) -> KeyMsg {
        KeyMsg {
            key: code,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn press(editor: &mut ConfigEditor, codes: &[KeyCode]) {
        for code in codes {
            let _ = editor.handle_key(key(*code));
        }
    }

    fn type_text(editor: &mut ConfigEditor, text: &str) {
        for ch in text.chars() {
            let _ = editor.handle_key(key(KeyCode::Char(ch)));
        }
    }

    fn move_to(editor: &mut ConfigEditor, field_key: &str) {
        let session = editor.session.as_mut().unwrap();
        session.cursor = session
            .rows
            .iter()
            .position(|index| session.view.fields[*index].key == field_key)
            .unwrap_or_else(|| panic!("{field_key} 행이 보여야 함"));
    }

    fn open_prod(editor: &mut ConfigEditor) {
        press(editor, &[KeyCode::Enter]);
        assert_eq!(editor.screen, Screen::Edit);
    }

    #[test]
    fn editing_a_number_field_sets_it_and_review_lists_it() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "keep_full");
        press(&mut editor, &[KeyCode::Enter]);
        for _ in 0..4 {
            press(&mut editor, &[KeyCode::Backspace]);
        }
        type_text(&mut editor, "7");
        press(&mut editor, &[KeyCode::Enter, KeyCode::Char('s')]);

        assert_eq!(editor.screen, Screen::Review);
        let change = editor.change.as_ref().unwrap();
        assert_eq!(
            change.set_rows(),
            vec![("keep_full".to_string(), "7".to_string())]
        );
    }

    #[test]
    fn invalid_number_stays_in_the_editor_with_an_error() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "keep_full");
        press(&mut editor, &[KeyCode::Enter, KeyCode::Backspace]);
        type_text(&mut editor, "x");
        press(&mut editor, &[KeyCode::Enter]);

        let session = editor.session.as_ref().unwrap();
        assert!(
            session.editor.is_some(),
            "잘못된 값이면 편집을 끝내지 않아야 함"
        );
        assert!(session.edits.is_empty());
        assert!(matches!(
            editor.notice.as_ref().map(|n| n.tone),
            Some(Tone::Danger)
        ));
    }

    #[test]
    fn d_on_a_direct_field_unsets_it() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "keep_full");
        press(&mut editor, &[KeyCode::Char('d'), KeyCode::Char('s')]);

        let change = editor.change.as_ref().unwrap();
        assert_eq!(change.unset_rows(), vec!["keep_full".to_string()]);
    }

    #[test]
    fn empty_input_on_an_inherited_field_keeps_inheritance() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "compress_level");
        press(&mut editor, &[KeyCode::Enter, KeyCode::Enter]);

        assert!(editor.session.as_ref().unwrap().edits.is_empty());
    }

    fn editor_value(editor: &ConfigEditor) -> String {
        match editor
            .session
            .as_ref()
            .and_then(|session| session.editor.as_ref())
        {
            Some(FieldEditor::Text(input)) => input.value(),
            Some(FieldEditor::Choice { options, index }) => options[*index].to_string(),
            None => panic!("편집 중이어야 함"),
        }
    }

    #[test]
    fn choice_starts_at_the_current_value_and_switching_overrides_it() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "encrypt");
        press(&mut editor, &[KeyCode::Enter]);
        let current = editor_value(&editor);
        assert!(
            current == "true" || current == "false",
            "현재 값에서 시작해야 함: {current}"
        );
        let target = if current == "true" { "false" } else { "true" };
        while editor_value(&editor) != target {
            press(&mut editor, &[KeyCode::Right]);
        }
        press(&mut editor, &[KeyCode::Enter, KeyCode::Char('s')]);

        let change = editor.change.as_ref().unwrap();
        assert_eq!(
            change.set_rows(),
            vec![("encrypt".to_string(), target.to_string())]
        );
    }

    #[test]
    fn text_fields_start_from_the_current_value_and_enter_keeps_it() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "keep_full");
        press(&mut editor, &[KeyCode::Enter]);
        assert_eq!(editor_value(&editor), "4", "직접 적힌 값");
        press(&mut editor, &[KeyCode::Enter]);

        move_to(&mut editor, "compress_level");
        press(&mut editor, &[KeyCode::Enter]);
        assert_eq!(editor_value(&editor), "3", "[defaults]에서 상속된 값");
        press(&mut editor, &[KeyCode::Enter]);

        assert!(
            editor.session.as_ref().unwrap().edits.is_empty(),
            "그대로 enter는 변경이 아님"
        );
    }

    #[test]
    fn stored_uri_is_prefilled_in_full_and_can_be_edited_or_cleared() {
        let raw = "mongodb://user:pw@db.internal:27017/app";
        let text = format!("[profile.prod]\nuri = \"{raw}\"\ndest = \"local:/tmp/x-backup-tui\"\n");
        let (_dir, mut editor) = editor(&text);
        open_prod(&mut editor);
        move_to(&mut editor, "uri");
        press(&mut editor, &[KeyCode::Enter]);
        assert_eq!(
            editor_value(&editor),
            raw,
            "가린 값이 아니라 원문을 채워야 함"
        );

        for _ in 0.."app".len() {
            press(&mut editor, &[KeyCode::Backspace]);
        }
        type_text(&mut editor, "orders");
        press(&mut editor, &[KeyCode::Enter]);
        assert_eq!(
            editor.session.as_ref().unwrap().edits.get("uri"),
            Some(&Edit::Value(
                "mongodb://user:pw@db.internal:27017/orders".to_string()
            ))
        );

        press(&mut editor, &[KeyCode::Enter]);
        for _ in 0..80 {
            press(&mut editor, &[KeyCode::Backspace]);
        }
        press(&mut editor, &[KeyCode::Enter]);
        assert_eq!(
            editor.session.as_ref().unwrap().edits.get("uri"),
            Some(&Edit::ClearUri),
            "직접 적힌 URI를 모두 지우면 키 삭제"
        );
    }

    #[test]
    fn uri_empty_is_no_change_and_clear_needs_a_direct_value() {
        let text = "[profile.prod]\nuri = \"mongodb://user:pw@db.internal:27017/app\"\n\
                    dest = \"local:/tmp/x-backup-tui\"\n";
        let (_dir, mut editor) = editor(text);
        open_prod(&mut editor);
        move_to(&mut editor, "uri");
        press(&mut editor, &[KeyCode::Enter, KeyCode::Enter]);
        assert!(editor.session.as_ref().unwrap().edits.is_empty());

        press(&mut editor, &[KeyCode::Char('d'), KeyCode::Char('s')]);
        let change = editor.change.as_ref().unwrap();
        assert!(change.unset_rows().contains(&"uri".to_string()));
    }

    #[test]
    fn uri_with_credentials_is_saved_with_a_warning_and_masked_in_review() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "uri");
        press(&mut editor, &[KeyCode::Enter]);
        type_text(&mut editor, "mongodb://admin:hunter2@db.internal:27017/app");
        press(&mut editor, &[KeyCode::Enter]);

        let session = editor.session.as_ref().unwrap();
        assert!(
            session.editor.is_none(),
            "자격증명이 든 URI도 확정되어야 함"
        );
        assert!(matches!(
            editor.notice.as_ref().map(|n| n.tone),
            Some(Tone::Warn)
        ));
        assert!(
            !editor.view().contains("hunter2"),
            "확정 뒤 목록에서는 가려야 함"
        );

        press(&mut editor, &[KeyCode::Char('s')]);
        assert_eq!(editor.screen, Screen::Review);
        let change = editor.change.as_ref().unwrap();
        assert_eq!(
            change.set.get("uri").and_then(|v| v.as_str()),
            Some("mongodb://admin:hunter2@db.internal:27017/app")
        );
        let view = editor.view();
        assert!(view.contains("db.internal"));
        assert!(!view.contains("hunter2"), "검토 화면에서도 가려야 함");
    }

    #[test]
    fn uri_is_visible_while_typing() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "uri");
        press(&mut editor, &[KeyCode::Enter]);
        type_text(&mut editor, "mongodb://db.internal:27017/app");

        assert!(editor.view().contains("db.internal:27017"));
    }

    #[test]
    fn plain_uri_reaches_review() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "uri");
        press(&mut editor, &[KeyCode::Enter]);
        type_text(&mut editor, "mongodb://db.internal:27017/app");
        press(&mut editor, &[KeyCode::Enter, KeyCode::Char('s')]);

        assert_eq!(editor.screen, Screen::Review);
        assert!(editor.view().contains("db.internal:27017"));
    }

    #[test]
    fn escape_with_pending_edits_needs_a_second_press() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "keep_full");
        press(&mut editor, &[KeyCode::Char('d'), KeyCode::Esc]);
        assert_eq!(editor.screen, Screen::Edit);
        press(&mut editor, &[KeyCode::Esc]);
        assert_eq!(editor.screen, Screen::Profiles);
        assert!(editor.session.is_none());
    }

    #[test]
    fn new_profile_without_a_source_is_refused_at_review() {
        let (_dir, mut editor) = editor(SAMPLE);
        press(&mut editor, &[KeyCode::Char('n')]);
        type_text(&mut editor, "staging");
        press(&mut editor, &[KeyCode::Enter]);
        assert_eq!(editor.screen, Screen::Edit);

        press(&mut editor, &[KeyCode::Char('s')]);
        assert_eq!(editor.screen, Screen::Edit);
        assert!(matches!(
            editor.notice.as_ref().map(|n| n.tone),
            Some(Tone::Danger)
        ));

        move_to(&mut editor, "uri_env");
        press(&mut editor, &[KeyCode::Enter]);
        type_text(&mut editor, "STAGING_URI");
        press(&mut editor, &[KeyCode::Enter, KeyCode::Char('s')]);
        assert_eq!(editor.screen, Screen::Review);
        assert_eq!(editor.change.as_ref().unwrap().op, ChangeOp::Create);
    }

    #[test]
    fn korean_profile_name_is_refused_with_a_hint_instead_of_crashing() {
        let (_dir, mut editor) = editor(SAMPLE);
        press(&mut editor, &[KeyCode::Char('n')]);
        type_text(&mut editor, "운영");
        let view = editor.view();

        assert_eq!(editor.screen, Screen::NewName);
        assert_eq!(
            editor.name_input.value(),
            "",
            "허용되지 않는 글자는 입력되지 않아야 함"
        );
        let notice = editor
            .notice
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default();
        assert!(notice.contains('영'), "무엇을 막았는지 알려야 함: {notice}");
        assert!(view.contains("prod"), "쓸 수 있는 이름 예시를 보여야 함");
    }

    #[test]
    fn pasted_profile_name_keeps_only_allowed_characters() {
        let (_dir, mut editor) = editor(SAMPLE);
        press(&mut editor, &[KeyCode::Char('n')]);
        let _ = editor.paste(PasteMsg("운영-prod 2".to_string()));

        assert_eq!(editor.name_input.value(), "-prod2");
        assert!(editor.notice.is_some());
    }

    #[test]
    fn non_ascii_field_value_renders_without_crashing() {
        let (_dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "dest");
        press(&mut editor, &[KeyCode::Enter]);
        type_text(&mut editor, "/백업");
        press(&mut editor, &[KeyCode::Left, KeyCode::Left]);

        let view = editor.view();
        assert!(view.contains("백"), "한글 경로가 그대로 보여야 함");
    }

    #[test]
    fn pointing_dest_at_another_profiles_directory_warns_on_commit_and_in_review() {
        let text = r#"
[profile.prod]
uri_env = "XB_CONFIG_TUI_TEST_URI"
dest = "local:/tmp/x-backup-tui-prod"

[profile.staging]
uri_env = "XB_CONFIG_TUI_TEST_URI"
dest = "local:/tmp/x-backup-tui-staging"
"#;
        let (_dir, mut editor) = editor(text);
        press(&mut editor, &[KeyCode::Down, KeyCode::Enter]);
        assert_eq!(editor.session.as_ref().unwrap().name.as_str(), "staging");
        move_to(&mut editor, "dest");
        press(&mut editor, &[KeyCode::Enter]);
        for _ in 0.."staging".len() {
            press(&mut editor, &[KeyCode::Backspace]);
        }
        type_text(&mut editor, "prod");
        press(&mut editor, &[KeyCode::Enter]);

        let notice = editor.notice.as_ref().map(|n| (n.tone, n.text.clone()));
        assert!(
            matches!(&notice, Some((Tone::Warn, text)) if text.contains("prod")),
            "다른 프로파일 이름을 알려야 함: {notice:?}"
        );
        press(&mut editor, &[KeyCode::Char('s')]);
        assert_eq!(editor.screen, Screen::Review);
        assert!(editor.view().contains("same backup location"));
    }

    #[test]
    fn duplicate_profile_name_is_rejected() {
        let (_dir, mut editor) = editor(SAMPLE);
        press(&mut editor, &[KeyCode::Char('n')]);
        type_text(&mut editor, "prod");
        press(&mut editor, &[KeyCode::Enter]);
        assert_eq!(editor.screen, Screen::NewName);
        assert!(editor.notice.is_some());
    }

    #[test]
    fn destination_group_is_hidden_when_not_editable() {
        let text = r#"
default_profile = "prod"

[profile.prod]
uri_env = "XB_CONFIG_TUI_TEST_URI"

[[profile.prod.dest]]
name = "primary"
dest = "local:/tmp/a"

[[profile.prod.dest]]
name = "offsite"
dest = "local:/tmp/b"
"#;
        let (_dir, mut editor) = editor(text);
        open_prod(&mut editor);
        let session = editor.session.as_ref().unwrap();
        assert!(!session.view.destination_editable);
        assert!(session
            .rows
            .iter()
            .all(|index| session.view.fields[*index].group != FieldGroup::Destination));
    }

    #[test]
    fn connection_test_builds_a_draft_without_writing_the_file() {
        let (dir, mut editor) = editor(SAMPLE);
        open_prod(&mut editor);
        move_to(&mut editor, "keep_full");
        press(&mut editor, &[KeyCode::Char('d'), KeyCode::Char('s')]);
        let before = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();

        let cmd = editor.handle_key(key(KeyCode::Char('t')));
        assert!(cmd.is_some(), "연결 테스트 명령이 나와야 함");
        assert!(matches!(editor.test, TestState::Running));
        let after = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert_eq!(before, after, "연결 테스트는 파일을 쓰지 않아야 함");
    }

    #[test]
    fn fit_tail_keeps_the_file_name() {
        let shown = fit_tail("/very/long/directory/tree/config.toml", 16);
        assert_eq!(lipgloss::width(&shown), 16);
        assert!(shown.starts_with('…') && shown.ends_with("config.toml"));
        assert_eq!(fit_tail("/a/config.toml", 40), "/a/config.toml");
    }

    #[test]
    fn fit_respects_double_width_characters() {
        assert_eq!(lipgloss::width(&fit("백업위치설정", 7)), 7);
        assert_eq!(fit("abc", 5), "abc  ");
        assert!(fit("abcdefgh", 5).ends_with('…'));
    }

    #[test]
    fn build_report_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let future = crate::cli::handlers::status::build_report(None, "prod", Lang::En);
        assert_send(&future);
    }
}
