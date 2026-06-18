use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use bstr::ByteVec;
use camino::Utf8PathBuf;
use compact_str::CompactString;
use fs_err::{self as fs};
use itertools::Either;
use ratatui::{
    layout::Constraint,
    style::{Style, Stylize},
    text::Text,
    widgets::{Row, Table},
};
use tracing::{error, warn};
use tui_input::Input;

use crate::{
    keybinds::Keybinds,
    traits::{HasEscapedBytes, LastIndex},
    tui::single_line_selector::SingleLineSelectorState,
};

mod macro_nametag;
pub use macro_nametag::MacroNameTag;
mod tui;

// #[derive(Debug)]
// #[repr(u8)]
// pub enum MacrosPrompt {
//     None,
//     Delete,
//     AddEdit(MacroEditing),
// }

pub const MACROS_DIR_PATH: &str = "macros";

pub enum MacroCategorySelection<'a> {
    AllMacros,
    StringsOnly,
    WithBytes,
    NoCategory,
    Category(&'a str),
}

#[derive(Debug, thiserror::Error)]
#[error("Macro not found!")]
pub struct MacroNotFound;

pub struct Macros {
    pub all: BTreeMap<MacroNameTag, MacroContent>,

    pub categories_selector: SingleLineSelectorState,

    // TODO make private?
    pub search_input: Input,
}

#[derive(Debug, thiserror::Error)]
#[error("failed reading macro file")]
pub struct MacrosLoadError(#[from] std::io::Error);

#[derive(Debug, thiserror::Error)]
#[error("invalid macro in: {path}")]
pub struct MacrosDeserError {
    path: Utf8PathBuf,
    source: toml::de::Error,
}

impl Macros {
    pub fn empty() -> Self {
        Self {
            // scrollbar_state: ScrollbarState::new(test_macros.len()),
            all: BTreeMap::new(),
            // tx_queue: Vec::new(),
            // ui_state: MacrosPrompt::None,
            search_input: Input::default(),
            categories_selector: SingleLineSelectorState::new().with_selected(2),
            // categories: BTreeSet::new(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }
    pub fn visible_len(&self) -> usize {
        if self.is_empty() {
            0
        } else {
            self.filtered_macro_iter().count()
        }
    }
    pub fn none_visible(&self) -> bool {
        self.visible_len() == 0
    }
    fn selected_category(&self) -> MacroCategorySelection<'_> {
        match self.categories_selector.current_index {
            0 => MacroCategorySelection::WithBytes,
            1 => MacroCategorySelection::StringsOnly,
            2 => MacroCategorySelection::AllMacros,
            3 if self.has_no_category_macros() => MacroCategorySelection::NoCategory,
            index => MacroCategorySelection::Category(
                self.categories().nth(index - 3).unwrap_or("?????"),
            ),
        }
    }
    /// Return an iterator of all Macros, filtered by the selected category in the UI
    fn filtered_by_category(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&MacroNameTag, &MacroContent)> {
        let category = self.selected_category();

        self.all
            .iter()
            .filter(move |(tag, content)| match category {
                MacroCategorySelection::AllMacros => true,
                MacroCategorySelection::StringsOnly => !content.has_escaped_bytes,
                MacroCategorySelection::WithBytes => content.has_escaped_bytes,
                MacroCategorySelection::NoCategory => tag.category.is_none(),
                MacroCategorySelection::Category(cat) => tag.category.as_deref() == Some(cat),
            })
    }
    /// Return an iterator of all Macros, filtered by the entered search query, case-insensitive.
    fn filtered_by_search(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&MacroNameTag, &MacroContent)> {
        let query = self.search_input.value();
        let query_len = query.len();
        self.all.iter().filter(move |(tag, _)| {
            if tag.name.is_char_boundary(query_len) {
                tag.name[..query_len].eq_ignore_ascii_case(query)
            } else {
                false
            }
        })
    }
    pub fn filtered_macro_iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&MacroNameTag, &MacroContent)> {
        if self.search_input.value().is_empty() {
            Either::Right(self.filtered_by_category())
        } else {
            Either::Left(self.filtered_by_search())
        }
    }
    pub fn as_table(&self, keybinds: &Keybinds, fuzzy_macro_name_match: bool) -> Table<'_> {
        let filtered = self
            .filtered_macro_iter()
            .map(|(tag, _)| {
                let mut single_action_keys = keybinds
                    .keybindings
                    .iter()
                    .filter(|(_, km)| km.len() == 1)
                    .map(|(kc, km)| (kc, &km[0]));

                let keybind_for_macro = single_action_keys
                    .find(|(_, km)| {
                        let Ok(keybind_as_tag) = km.parse::<MacroNameTag>() else {
                            return false;
                        };
                        &keybind_as_tag == tag
                    })
                    .or_else(|| {
                        if fuzzy_macro_name_match {
                            single_action_keys.find(|(_, km)| {
                                let Ok(keybind_as_tag) = km.parse::<MacroNameTag>() else {
                                    return false;
                                };

                                keybind_as_tag.eq_fuzzy(tag)
                            })
                        } else {
                            None
                        }
                    });
                let macro_string = keybind_for_macro.map(|(kc, _)| kc.to_string());

                (tag.name.as_str(), macro_string.unwrap_or_default())
            })
            .map(|(m, k)| Row::new([Text::raw(m), Text::raw(k).italic()]));

        let widths = [Constraint::Fill(4), Constraint::Fill(1)];
        Table::new(filtered, widths).row_highlight_style(Style::new().reversed())
    }
    pub fn has_no_category_macros(&self) -> bool {
        self.all.iter().any(|(tag, _)| tag.category.is_none())
    }
    pub fn categories(&self) -> impl DoubleEndedIterator<Item = &str> {
        let no_category = std::iter::once("No Category").filter(|_| self.has_no_category_macros());

        let categories: BTreeSet<&str> = self
            .all
            .keys()
            .filter_map(|tag| tag.category.as_deref())
            .collect();

        no_category.chain(categories)
    }
    pub fn get_by_string(&self, query: &str, fuzzy_macro_name_match: bool) -> Option<MacroNameTag> {
        let query_nametag: MacroNameTag = query.parse().ok()?;

        let find_result = self
            .all
            .iter()
            .find(|(tag, _)| query_nametag.eq(tag))
            .map(|(t, _)| t.clone());

        match find_result {
            None if fuzzy_macro_name_match => self
                .all
                .iter()
                .find(|(tag, _)| query_nametag.eq_fuzzy(tag))
                .map(|(t, _)| t.clone()),
            None => None,
            Some(tag) => Some(tag),
        }
    }
    // pub fn macro_from_key_combo<'a>(
    //     &'a self,
    //     key_combo: KeyCombination,
    //     macro_keybinds: &'a IndexMap<KeyCombination, Vec<MacroNameTag>>,
    //     fuzzy_macro_name_match: bool,
    // ) -> Result<Vec<&'a MacroNameTag>, Option<Vec<&'a MacroNameTag>>> {
    //     // ) -> Result<Vec<usize>, Option<Vec<&KeybindMacro>>> {
    //     let Some(v) = macro_keybinds.get(&key_combo) else {
    //         return Err(None);
    //     };

    //     let mut somes: Vec<&MacroNameTag> = Vec::new();
    //     let mut nones: Vec<&MacroNameTag> = Vec::new();

    //     v.iter().for_each(|config_tag| {
    //         let eq_result = self
    //             .all
    //             .iter()
    //             // .enumerate()
    //             .find(|(tag, content)| config_tag.eq(tag));
    //         match eq_result {
    //             Some((tag, content)) => {
    //                 somes.push(tag);
    //                 return;
    //             }
    //             None if fuzzy_macro_name_match => (),
    //             None => {
    //                 nones.push(config_tag);
    //                 return;
    //             }
    //         }
    //         assert!(fuzzy_macro_name_match);
    //         let eq_result = self
    //             .all
    //             .iter()
    //             // .enumerate()
    //             .find(|(tag, content)| config_tag.eq_fuzzy(tag));
    //         match eq_result {
    //             Some((tag, content)) => somes.push(tag),
    //             None => nones.push(config_tag),
    //         }
    //     });

    //     if nones.is_empty() {
    //         Ok(somes)
    //     } else {
    //         Err(Some(nones))
    //     }
    // }

    // pub fn remove_macro(&mut self, macro_ref: &MacroNameTag) {
    //     self.all
    //         .remove(macro_ref)
    //         .expect("attempted removal of non-existant element");
    // }

    pub fn load_from_folder<P: AsRef<Path>>(
        folder: P,
    ) -> Result<(Self, Vec<MacrosDeserError>), MacrosLoadError> {
        let mut instance = Macros::empty();
        let mut deser_errors = Vec::new();
        fn visit_dir(
            dir: &Path,
            new_macros: &mut BTreeMap<MacroNameTag, MacroContent>,
            deser_errors: &mut Vec<MacrosDeserError>,
        ) -> Result<(), MacrosLoadError> {
            for entry in fs::read_dir(dir)? {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        error!("Folder iteration error: {e}");
                        return Err(e.into());
                    }
                };

                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(e) => {
                        error!("Failed to get metadata for {}: {e}", entry.path().display());
                        return Err(e.into());
                    }
                };

                if metadata.is_dir() {
                    // Recurse into subdirectory
                    if let Err(e) = visit_dir(&entry.path(), new_macros, deser_errors) {
                        error!(
                            "Error traversing subdirectory {}: {e}",
                            entry.path().display()
                        );
                        // propagate error for now
                        return Err(e);
                    }
                    continue;
                }

                if !metadata.is_file() {
                    continue;
                }

                let Ok(file_path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                    warn!(
                        "Macro path \"{}\" is not valid UTF-8! Skipping...",
                        entry.path().display()
                    );
                    continue;
                };

                if let Some(extension) = file_path.extension() {
                    if extension != "toml" {
                        continue;
                    }
                } else {
                    continue;
                }

                let file_contents = fs::read_to_string(&file_path)?;

                let mut deserialized: MacroFile = match toml::from_str(&file_contents) {
                    Ok(file) => file,
                    Err(e) => {
                        error!("Failed to read macros from file: {file_path}. {e}");
                        deser_errors.push(MacrosDeserError {
                            path: file_path,
                            source: e,
                        });
                        continue;
                    }
                };

                // If a macro has no category set, use either the stem of the file
                // or the override if one was provided.
                let fallback_category = deserialized
                    .category_override
                    .as_ref()
                    .map(CompactString::as_str)
                    .unwrap_or_else(|| {
                        file_path
                            .file_stem()
                            .expect("expected to remove toml extension")
                    });

                deserialized
                    .macros
                    .iter_mut()
                    .filter(|m| m.category.is_none())
                    .for_each(|m| m.category = Some(fallback_category.into()));

                for ser_macro in deserialized.macros {
                    let (mut tag, content) = ser_macro.into_tag_and_content();

                    if let Some(category) = &mut tag.category
                        && category.trim().is_empty()
                    {
                        tag.category = None;
                    }

                    if let Some(_old) = new_macros.get(&tag) {
                        warn!("Duplicate found for macro {tag}!")
                    }
                    _ = new_macros.insert(tag, content);
                }
            }
            Ok(())
        }
        let folder = folder.as_ref();

        if folder.exists() {
            let mut new_macros = BTreeMap::new();
            visit_dir(folder, &mut new_macros, &mut deser_errors)?;
            instance.all = new_macros;
        } else {
            fs::create_dir_all(folder)?;
        }
        Ok((instance, deser_errors))
    }
}

impl LastIndex for Macros {
    fn last_index_checked(&self) -> Option<usize> {
        let visible = self.visible_len();
        if visible == 0 {
            None
        } else {
            Some(visible - 1)
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct MacroFile {
    #[serde(default)]
    #[serde(alias = "name")]
    #[serde(alias = "category")]
    category_override: Option<CompactString>,
    #[serde(rename = "macro")]
    macros: Vec<SerializedMacro>,
}
#[derive(Debug, serde::Deserialize)]
struct SerializedMacro {
    name: CompactString,
    category: Option<CompactString>,
    content: CompactString,
    line_ending: Option<CompactString>,
    #[serde(default)]
    sensitive: bool,
}
impl SerializedMacro {
    fn into_tag_and_content(self) -> (MacroNameTag, MacroContent) {
        let SerializedMacro {
            name,
            category,
            content,
            line_ending,
            sensitive,
        } = self;

        (
            MacroNameTag { name, category },
            MacroContent::new_with_line_ending(&content, line_ending, sensitive),
        )
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MacroContent {
    pub content: CompactString,
    pub has_escaped_bytes: bool,
    // #[serde(skip_serializing_if = "Option::is_none")]
    pub escaped_line_ending: Option<CompactString>,
    pub sensitive: bool,
}

impl MacroContent {
    // pub fn new<S: AsRef<str>>(value: S) -> Self {
    //     Self {
    //         has_bytes: value.as_ref().has_escaped_bytes(),
    //         content: value.as_ref().into(),
    //         line_ending: None,
    //     }
    // }
    // pub fn update(&mut self, new: CompactString) {
    //     self.content = new;
    //     self.has_bytes = self.content.has_escaped_bytes();
    // }
    pub fn new_with_line_ending<S: AsRef<str>>(
        value: S,
        escaped_line_ending: Option<CompactString>,
        sensitive: bool,
    ) -> Self {
        Self {
            has_escaped_bytes: value.as_ref().has_escaped_bytes(),
            content: value.as_ref().into(),
            escaped_line_ending,
            sensitive,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
    pub fn unescape_bytes(&self) -> Vec<u8> {
        Vec::unescape_bytes(&self.content)
    }
    pub fn as_str(&self) -> &str {
        &self.content
    }
}
