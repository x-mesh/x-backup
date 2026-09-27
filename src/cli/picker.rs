//! 인터랙티브 백업 선택 — `restore --id` 미지정 + TTY에서 Bubble Tea TUI를 연다.
//!
//! 후보는 Full + Complete만 최신순으로 보여준다. Model-View-Update 상태는 검색어, 필터 결과,
//! 선택 행, 터미널 크기를 함께 관리한다. 대체 화면을 사용하므로 종료 후 기존 CLI 출력이 복원된다.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use bubbletea_rs::{quit, window_size, Cmd, KeyMsg, Model, Msg, PasteMsg, Program, WindowSizeMsg};
use bubbletea_widgets::table::{Column, Model as TableModel, Row, Styles as TableStyles};
use bubbletea_widgets::textinput::{self, Model as TextInput};
use crossterm::event::{KeyCode, KeyModifiers};
use lipgloss_extras::lipgloss::{AdaptiveColor, Style};

use crate::engine::mongo::status::human_bytes;
use crate::error::{Result, XBackupError};
use crate::manifest::schema::{BackupManifest, BackupStatus, BackupType};
use crate::manifest::store::ManifestStore;
use crate::pipeline::verify::collect_manifest_ids;
use crate::storage::Storage;

const ACCENT: AdaptiveColor = AdaptiveColor {
    Light: "#087F72",
    Dark: "#5EEAD4",
};
const TEXT: AdaptiveColor = AdaptiveColor {
    Light: "#17202A",
    Dark: "#E6EDF3",
};
const MUTED: AdaptiveColor = AdaptiveColor {
    Light: "#5A6772",
    Dark: "#8B9AAA",
};
const RULE: AdaptiveColor = AdaptiveColor {
    Light: "#C9D3DC",
    Dark: "#33404D",
};
const SELECTED_BG: AdaptiveColor = AdaptiveColor {
    Light: "#C8F3EC",
    Dark: "#123F3A",
};
const SELECTED_FG: AdaptiveColor = AdaptiveColor {
    Light: "#063C36",
    Dark: "#E9FFFB",
};

/// 피커에 보여줄 복구 후보 한 건.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupChoice {
    pub id: String,
    pub kind: String,
    pub engine: String,
    pub server_version: String,
    pub created_at: String,
    pub size: String,
    search_text: String,
}

/// 복구 베이스로 유효한 후보(Full + Complete)를 최신순으로 모은다.
pub async fn full_backup_choices(storage: &dyn Storage) -> Result<Vec<BackupChoice>> {
    let ids = collect_manifest_ids(storage).await?;
    let store = ManifestStore::new(storage);
    let mut manifests = Vec::with_capacity(ids.len());
    for id in ids {
        match store.read(&id).await {
            Ok(m) if m.backup_type == BackupType::Full && m.status == BackupStatus::Complete => {
                if m.id == id {
                    manifests.push(m);
                } else {
                    tracing::warn!(
                        storage_id = %id,
                        manifest_id = %m.id,
                        "manifest ID가 저장 경로와 달라 피커에서 제외"
                    );
                }
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(id = %id, "manifest 읽기 실패(피커에서 제외): {e}"),
        }
    }
    manifests.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(manifests.iter().map(format_choice).collect())
}

fn format_choice(manifest: &BackupManifest) -> BackupChoice {
    let engine = match manifest.tool_versions.archive_format.as_deref() {
        Some(format) if format.starts_with("xb-mysql") => "mysql",
        Some(format) if format.starts_with("xb-pg") => "postgresql",
        _ => "mongodb",
    };
    let kind = if manifest.promoted_from_gap {
        "full (gap)"
    } else {
        "full"
    };
    let size = human_bytes(manifest.stored_size_bytes as i64);
    let search_text = format!(
        "{} {kind} {engine} {} {} {size}",
        manifest.id, manifest.server_version, manifest.created_at
    )
    .to_ascii_lowercase();
    BackupChoice {
        id: manifest.id.clone(),
        kind: kind.to_string(),
        engine: engine.to_string(),
        server_version: manifest.server_version.clone(),
        created_at: manifest.created_at.clone(),
        size,
        search_text,
    }
}

#[derive(Clone)]
struct PickerSeed {
    choices: Vec<BackupChoice>,
    lang: crate::i18n::Lang,
}

static PICKER_SEED: OnceLock<Mutex<Option<PickerSeed>>> = OnceLock::new();

struct PickerModel {
    choices: Vec<BackupChoice>,
    filtered: Vec<usize>,
    table: TableModel,
    search: TextInput,
    search_active: bool,
    width: u16,
    height: u16,
    selected_id: Option<String>,
    cancelled: bool,
    lang: crate::i18n::Lang,
}

impl PickerModel {
    fn from_seed(seed: PickerSeed) -> Self {
        let mut search = textinput::new();
        search.prompt = "/ ".to_string();
        search.set_placeholder(seed.lang.sel("filter backups", "백업 검색"));
        search.prompt_style = Style::new().foreground(ACCENT).bold(true);
        search.text_style = Style::new().foreground(TEXT);
        search.placeholder_style = Style::new().foreground(MUTED);

        let mut model = Self {
            filtered: (0..seed.choices.len()).collect(),
            choices: seed.choices,
            table: TableModel::new(Vec::new()),
            search,
            search_active: false,
            width: 100,
            height: 28,
            selected_id: None,
            cancelled: false,
            lang: seed.lang,
        };
        model.sync_table(None);
        model
    }

    fn selected_choice(&self) -> Option<&BackupChoice> {
        self.filtered
            .get(self.table.selected)
            .and_then(|index| self.choices.get(*index))
    }

    fn apply_filter(&mut self) {
        let selected = self.selected_choice().map(|choice| choice.id.clone());
        let query = self.search.value().to_ascii_lowercase();
        let tokens: Vec<&str> = query.split_whitespace().collect();
        self.filtered = self
            .choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                tokens
                    .iter()
                    .all(|token| choice.search_text.contains(token))
            })
            .map(|(index, _)| index)
            .collect();
        self.sync_table(selected.as_deref());
    }

    fn sync_table(&mut self, selected_id: Option<&str>) {
        let id_width = shortest_unique_id_prefix(&self.choices, 8);
        self.table.columns = table_columns(self.width, id_width);
        self.table.selected = selected_id
            .and_then(|id| {
                self.filtered
                    .iter()
                    .position(|index| self.choices[*index].id == id)
            })
            .unwrap_or_else(|| {
                self.table
                    .selected
                    .min(self.filtered.len().saturating_sub(1))
            });
        self.refresh_rows();
        self.table.set_styles(TableStyles {
            header: Style::new()
                .bold(true)
                .foreground(MUTED)
                .padding(0, 1, 0, 1),
            cell: Style::new().foreground(TEXT).padding(0, 1, 0, 1),
            selected: Style::new()
                .bold(true)
                .foreground(SELECTED_FG)
                .background(SELECTED_BG),
        });
        self.table
            .set_width(i32::from(self.width.saturating_sub(4)));
        self.table
            .set_height(i32::from(self.height.saturating_sub(15).max(4)));
        self.table.update_viewport();
        self.search
            .set_width(i32::from(self.width.saturating_sub(22).max(12)));
    }

    fn refresh_rows(&mut self) {
        let id_width = shortest_unique_id_prefix(&self.choices, 8);
        self.table.rows = self
            .filtered
            .iter()
            .enumerate()
            .map(|(row, index)| {
                table_row(
                    &self.choices[*index],
                    self.width,
                    row == self.table.selected,
                    id_width,
                )
            })
            .collect();
        self.table.update_viewport();
    }

    fn update_search(&mut self, key: KeyMsg) -> Option<Cmd> {
        self.search_active = true;
        std::mem::drop(self.search.focus());
        let cmd = self.search.update(Box::new(key));
        self.apply_filter();
        cmd
    }

    fn cancel_or_clear(&mut self) -> Option<Cmd> {
        if self.search_active || !self.search.value().is_empty() {
            self.search.set_value("");
            self.search_active = false;
            self.search.blur();
            self.apply_filter();
            None
        } else {
            self.cancelled = true;
            Some(quit())
        }
    }
}

impl Model for PickerModel {
    fn init() -> (Self, Option<Cmd>) {
        let seed = PICKER_SEED
            .get_or_init(|| Mutex::new(None))
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
            .unwrap_or(PickerSeed {
                choices: Vec::new(),
                lang: crate::i18n::Lang::En,
            });
        (Self::from_seed(seed), Some(window_size()))
    }

    fn update(&mut self, msg: Msg) -> Option<Cmd> {
        if let Some(size) = msg.downcast_ref::<WindowSizeMsg>() {
            self.width = size.width.max(48);
            self.height = size.height.max(18);
            let selected = self.selected_choice().map(|choice| choice.id.clone());
            self.sync_table(selected.as_deref());
            return None;
        }
        if let Some(paste) = msg.downcast_ref::<PasteMsg>() {
            self.search_active = true;
            std::mem::drop(self.search.focus());
            let cmd = self.search.update(Box::new(paste.clone()));
            self.apply_filter();
            return cmd;
        }
        let key = msg.downcast_ref::<KeyMsg>().cloned()?;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.key == KeyCode::Char('c') {
            self.cancelled = true;
            return Some(quit());
        }
        match key.key {
            KeyCode::Enter => {
                if let Some(choice) = self.selected_choice() {
                    self.selected_id = Some(choice.id.clone());
                    return Some(quit());
                }
            }
            KeyCode::Esc => return self.cancel_or_clear(),
            KeyCode::Up => self.table.select_prev(),
            KeyCode::Down => self.table.select_next(),
            KeyCode::PageUp => self.table.move_up(self.table.height.max(1) as usize),
            KeyCode::PageDown => self.table.move_down(self.table.height.max(1) as usize),
            KeyCode::Home if !self.search_active => self.table.goto_top(),
            KeyCode::End if !self.search_active => self.table.goto_bottom(),
            KeyCode::Char('q') if !self.search_active => {
                self.cancelled = true;
                return Some(quit());
            }
            KeyCode::Char('/') if !self.search_active => {
                self.search_active = true;
                std::mem::drop(self.search.focus());
            }
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete if self.search_active => {
                return self.update_search(key);
            }
            KeyCode::Char(_) if !self.search_active => return self.update_search(key),
            _ => {}
        }
        self.refresh_rows();
        None
    }

    fn view(&self) -> String {
        render_picker(self)
    }
}

fn table_columns(width: u16, id_width: usize) -> Vec<Column> {
    if width >= 112 {
        vec![
            Column::new("CREATED (UTC)", 21),
            Column::new("TYPE", 10),
            Column::new("ENGINE", 12),
            Column::new("SIZE", 10),
            Column::new("BACKUP ID", 36),
        ]
    } else if width >= 78 {
        vec![
            Column::new("CREATED (UTC)", 21),
            Column::new("TYPE", 10),
            Column::new("ENGINE", 12),
            Column::new("SIZE", 10),
            Column::new("ID", id_width as i32),
        ]
    } else {
        vec![
            Column::new("CREATED", 16),
            Column::new("ENGINE", 10),
            Column::new("SIZE", 9),
            Column::new("ID", id_width as i32),
        ]
    }
}

fn table_row(choice: &BackupChoice, width: u16, selected: bool, id_width: usize) -> Row {
    let short_id: String = choice.id.chars().take(id_width).collect();
    let marker = if selected { "> " } else { "  " };
    if width >= 112 {
        Row::new(vec![
            format!("{marker}{}", short_created(&choice.created_at)),
            choice.kind.clone(),
            choice.engine.clone(),
            choice.size.clone(),
            choice.id.clone(),
        ])
    } else if width >= 78 {
        Row::new(vec![
            format!("{marker}{}", short_created(&choice.created_at)),
            choice.kind.clone(),
            choice.engine.clone(),
            choice.size.clone(),
            short_id,
        ])
    } else {
        Row::new(vec![
            format!("{marker}{}", compact_created(&choice.created_at)),
            choice.engine.clone(),
            choice.size.clone(),
            short_id,
        ])
    }
}

/// Git처럼 현재 후보를 구분할 수 있는 가장 짧은 ID 접두사를 고른다.
fn shortest_unique_id_prefix(choices: &[BackupChoice], minimum: usize) -> usize {
    let maximum = choices
        .iter()
        .map(|choice| choice.id.chars().count())
        .max()
        .unwrap_or(minimum);
    (minimum..=maximum)
        .find(|length| {
            let mut seen = HashSet::with_capacity(choices.len());
            choices
                .iter()
                .map(|choice| choice.id.chars().take(*length).collect::<String>())
                .all(|prefix| seen.insert(prefix))
        })
        .unwrap_or(maximum)
}

fn short_created(timestamp: &str) -> String {
    timestamp.replacen('T', " ", 1).chars().take(19).collect()
}

fn compact_created(timestamp: &str) -> String {
    let full = short_created(timestamp);
    full.get(5..16).unwrap_or(&full).to_string()
}

fn render_picker(model: &PickerModel) -> String {
    let horizontal = i32::from(model.width.saturating_sub(4));
    let title = Style::new()
        .bold(true)
        .foreground(TEXT)
        .render(model.lang.sel("Restore a backup", "복구할 백업 선택"));
    let count = Style::new().foreground(MUTED).render(&format!(
        "{} / {} {}",
        model.filtered.len(),
        model.choices.len(),
        model.lang.sel("backups", "개 백업")
    ));
    let rule = Style::new()
        .foreground(RULE)
        .render(&"─".repeat(horizontal.max(1) as usize));

    let search = if model.search_active || !model.search.value().is_empty() {
        model.search.view()
    } else {
        format!(
            "{} {}",
            Style::new().bold(true).foreground(ACCENT).render("/"),
            Style::new().foreground(MUTED).render(model.lang.sel(
                "Search by ID, engine, date, or version",
                "ID, 엔진, 날짜, 버전으로 검색"
            ))
        )
    };

    let body = if model.filtered.is_empty() {
        Style::new()
            .width(horizontal)
            .height(i32::from(model.height.saturating_sub(15).max(4)))
            .foreground(MUTED)
            .render(model.lang.sel(
                "No backups match this filter. Press Esc to clear it.",
                "검색 결과가 없습니다. Esc를 눌러 검색어를 지우세요.",
            ))
    } else {
        model.table.view()
    };

    let detail = model.selected_choice().map_or_else(String::new, |choice| {
        let latest = model.filtered.first().is_some_and(|index| {
            model.choices[*index].id == choice.id && model.search.value().is_empty()
        });
        let badge = if latest {
            format!(
                "  {}",
                Style::new()
                    .bold(true)
                    .foreground(ACCENT)
                    .render("[LATEST]")
            )
        } else {
            String::new()
        };
        format!(
            "{}{}\n{}  {}\n{}  {}",
            Style::new()
                .bold(true)
                .foreground(TEXT)
                .render(model.lang.sel("Selected backup", "선택한 백업")),
            badge,
            Style::new().foreground(MUTED).render("id"),
            Style::new().foreground(TEXT).render(&choice.id),
            Style::new().foreground(MUTED).render("server"),
            Style::new().foreground(TEXT).render(&format!(
                "{} · {} · {}",
                choice.server_version,
                short_created(&choice.created_at),
                choice.size
            ))
        )
    });

    let help = if model.search_active {
        model.lang.sel(
            "type filter  ↑↓ move  enter restore  esc clear",
            "입력 검색  ↑↓ 이동  enter 복구  esc 검색 해제",
        )
    } else {
        model.lang.sel(
            "/ search  ↑↓ move  enter restore  esc/q cancel",
            "/ 검색  ↑↓ 이동  enter 복구  esc/q 취소",
        )
    };

    Style::new()
        .width(i32::from(model.width.saturating_sub(2)))
        .padding(1, 1, 0, 1)
        .render(&format!(
            "{title}  {count}\n{rule}\n{search}\n\n{body}\n\n{detail}\n\n{}",
            Style::new().foreground(MUTED).render(help)
        ))
}

/// Bubble Tea 피커를 열어 백업 하나를 고른다. Esc/q/Ctrl-C는 `None`을 반환한다.
pub async fn pick_backup(
    choices: &[BackupChoice],
    lang: crate::i18n::Lang,
) -> Result<Option<String>> {
    let seed = PICKER_SEED.get_or_init(|| Mutex::new(None));
    *seed.lock().map_err(|_| {
        XBackupError::Failure(crate::tr!(
            "failed to lock the backup picker state",
            "백업 선택기 상태 잠금 실패"
        ))
    })? = Some(PickerSeed {
        choices: choices.to_vec(),
        lang,
    });
    let program = Program::<PickerModel>::builder()
        .alt_screen(true)
        .bracketed_paste(true)
        .build()
        .map_err(|e| {
            XBackupError::Usage(crate::tr!(
                "failed to start the backup picker: {e}",
                "백업 선택기 시작 실패: {e}"
            ))
        })?;
    let model = program.run().await.map_err(|e| {
        XBackupError::Usage(crate::tr!(
            "failed to read the backup selection: {e}",
            "백업 선택 입력 실패: {e}"
        ))
    })?;
    if model.cancelled {
        Ok(None)
    } else {
        Ok(model.selected_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{ToolVersions, Topology, FORMAT_VERSION};
    use crate::storage::LocalFs;

    fn manifest(
        id: &str,
        created_at: &str,
        ty: BackupType,
        status: BackupStatus,
    ) -> BackupManifest {
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: id.to_string(),
            created_at: created_at.to_string(),
            backup_type: ty,
            base_id: None,
            topology: Topology::ReplicaSet,
            server_version: "10.11.9-MariaDB".to_string(),
            tool_versions: ToolVersions {
                archive_format: Some("xb-mysql-v1".to_string()),
                ..ToolVersions::default()
            },
            selective: false,
            original_size_bytes: 100,
            stored_size_bytes: 2_000_000,
            compression: None,
            encryption: None,
            checksum_sha256: "abc".to_string(),
            oplog_range: None,
            oplog_count: None,
            promoted_from_gap: false,
            mysql_binlog: None,
            status,
        }
    }

    async fn write(fs: &LocalFs, manifest: &BackupManifest) {
        ManifestStore::new(fs).write(manifest).await.unwrap();
    }

    #[tokio::test]
    async fn collects_full_complete_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        write(
            &fs,
            &manifest(
                "a-old",
                "2026-06-10T00:00:00Z",
                BackupType::Full,
                BackupStatus::Complete,
            ),
        )
        .await;
        write(
            &fs,
            &manifest(
                "b-new",
                "2026-06-14T00:00:00Z",
                BackupType::Full,
                BackupStatus::Complete,
            ),
        )
        .await;
        write(
            &fs,
            &manifest(
                "c-incr",
                "2026-06-15T00:00:00Z",
                BackupType::Incremental,
                BackupStatus::Complete,
            ),
        )
        .await;
        let choices = full_backup_choices(&fs).await.unwrap();
        let ids: Vec<&str> = choices.iter().map(|choice| choice.id.as_str()).collect();
        assert_eq!(ids, vec!["b-new", "a-old"]);
    }

    #[test]
    fn mysql_choice_is_not_labeled_mongodb() {
        let choice = format_choice(&manifest(
            "bk-1",
            "2026-06-14T14:56:11Z",
            BackupType::Full,
            BackupStatus::Complete,
        ));
        assert_eq!(choice.engine, "mysql");
        assert!(choice.search_text.contains("mariadb"));
    }

    #[test]
    fn filtering_matches_multiple_metadata_tokens() {
        let choices = vec![format_choice(&manifest(
            "bk-1",
            "2026-06-14T14:56:11Z",
            BackupType::Full,
            BackupStatus::Complete,
        ))];
        let mut model = PickerModel::from_seed(PickerSeed {
            choices,
            lang: crate::i18n::Lang::En,
        });
        model.search.set_value("mysql 2026-06");
        model.apply_filter();
        assert_eq!(model.filtered, vec![0]);
        model.search.set_value("postgresql");
        model.apply_filter();
        assert!(model.filtered.is_empty());
    }

    #[test]
    fn id_prefix_expands_until_every_choice_is_distinct() {
        let choices = [
            "01a05f7f-d799-7df2-9983-2f7fae0d6ff3",
            "01a05f7f-add7-7112-907c-340239cf8d5d",
        ]
        .iter()
        .map(|id| {
            format_choice(&manifest(
                id,
                "2026-06-14T14:56:11Z",
                BackupType::Full,
                BackupStatus::Complete,
            ))
        })
        .collect::<Vec<_>>();
        assert_eq!(shortest_unique_id_prefix(&choices, 8), 10);
        assert_eq!(
            table_row(&choices[0], 100, false, 10).cells[4],
            "01a05f7f-d"
        );
        assert_eq!(
            table_row(&choices[1], 100, false, 10).cells[4],
            "01a05f7f-a"
        );
    }

    #[test]
    fn selection_marker_is_part_of_created_cell() {
        let choice = format_choice(&manifest(
            "bk-1",
            "2026-06-14T14:56:11Z",
            BackupType::Full,
            BackupStatus::Complete,
        ));
        let row = table_row(&choice, 100, true, 8);
        assert_eq!(row.cells.len(), 5);
        assert!(row.cells[0].starts_with("> 2026-06-14"));
    }

    #[test]
    fn render_has_empty_state_and_keyboard_help() {
        let mut model = PickerModel::from_seed(PickerSeed {
            choices: Vec::new(),
            lang: crate::i18n::Lang::Ko,
        });
        model.search.set_value("missing");
        model.apply_filter();
        let view = model.view();
        assert!(view.contains("검색 결과가 없습니다"));
        assert!(view.contains("enter"));
    }
}
