// Since I can't rely on prefix from strum
// #[strum(prefix = "show-")] doesn't work with FromStr :(
#![allow(clippy::enum_variant_names)]
// For RequiresPort, makes more sense to me to keep same format.
#![allow(clippy::match_like_matches_macro)]

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use compact_str::{CompactString, ToCompactString};
use crokey::KeyCombination;
use crossterm::event::{KeyCode, KeyModifiers};
use fs_err as fs;
use indexmap::IndexMap;
use serde::Deserialize;
use strum::{EnumMessage, VariantArray};

#[cfg(feature = "macros")]
use crate::macros::MacroNameTag;
use crate::{config_adjacent_path, traits::RequiresPort};

#[derive(
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
    strum::VariantArray,
    strum::EnumMessage,
)]
#[strum(serialize_all = "kebab-case")]
#[strum(ascii_case_insensitive)]
#[repr(u8)]
/// Actions to show settings/tools popups.
pub enum ShowPopupAction {
    /// Show all current keybinds, highlighting unrecognized actions.
    ShowKeybinds,
    #[strum(serialize = "show-portsettings")]
    /// Open the Port Settings menu.
    ShowPortSettings,
    /// Open the Behavior Settings menu.
    ShowBehavior,
    /// Open the Rendering Settings menu.
    ShowRendering,
    #[cfg(feature = "macros")]
    /// Open the Macros menu.
    ShowMacros,
    #[cfg(feature = "espflash")]
    #[strum(serialize = "show-espflash")]
    /// Open the espflash menu.
    ShowEspFlash,
    #[cfg(feature = "logging")]
    /// Open the Logging Settings menu.
    ShowLogging,
    #[cfg(feature = "defmt")]
    /// Open the defmt Settings menu.
    ShowDefmt,
}

impl RequiresPort for ShowPopupAction {
    fn requires_connection(&self) -> bool {
        // Requires no port presence, only UI actions.
        false
    }
}

// impl From<ShowPopupAction> for PopupMenu {
//     fn from(value: ShowPopupAction) -> Self {
//         match value {
//             ShowPopupAction::ShowBehavior => Self::BehaviorSettings,
//             ShowPopupAction::ShowRendering => Self::RenderingSettings,
//             ShowPopupAction::ShowPortSettings => Self::PortSettings,
//             #[cfg(feature = "logging")]
//             ShowPopupAction::ShowLogging => Self::Logging,
//             #[cfg(feature = "espflash")]
//             ShowPopupAction::ShowEspFlash => Self::EspFlash,
//             #[cfg(feature = "macros")]
//             ShowPopupAction::ShowMacros => Self::Macros,
//             #[cfg(feature = "defmt")]
//             ShowPopupAction::ShowDefmt => Self::Defmt,
//         }
//     }
// }

#[derive(
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
    strum::VariantArray,
    strum::EnumMessage,
)]
#[strum(serialize_all = "kebab-case")]
#[strum(ascii_case_insensitive)]
/// Actions for interacting with the core of the application,
/// regardless of features.
pub enum BaseAction {
    /// Toggle wrapping text longer than the screen.
    ToggleTextwrap,
    /// Toggle displaying timestamps.
    ToggleTimestamps,
    /// Toggle displaying line index and length in buffer.
    ToggleIndices,
    /// Toggle displaying index and length in hexadecimal format.
    ToggleIndicesHex,
    /// Toggle displaying recieved bytes in a Hex+ASCII view.
    ToggleHexView,
    /// Toggle displaying Address+Offset Markers+ASCII label above hex view.
    ToggleHexViewHeader,
    /// Toggle the ability to type into a text buffer before sending to device.
    TogglePseudoShell,
    /// Toggle whether the text buffer accepts bytes in Hex format, or plain text.
    TogglePseudoShellHex,
    /// Reload all Color Rules.
    ReloadColors,
    /// Reload all Keybinds.
    ReloadKeybinds,
    /// Escape a Keypress to avoid sending a key to the device to trigger an app menu or action.
    EscapeKeypress,
}

impl RequiresPort for BaseAction {
    fn requires_connection(&self) -> bool {
        false
    }
    fn requires_terminal_view(&self) -> bool {
        match self {
            // Has no use when not in the terminal view,
            // and behavior doesn't rely on port having healthy connection.
            BaseAction::EscapeKeypress => true,
            _ => false,
        }
    }
}

#[derive(
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
    strum::VariantArray,
    strum::EnumMessage,
)]
#[strum(serialize_all = "kebab-case")]
#[strum(ascii_case_insensitive)]
/// Actions to interact with a connected device.
pub enum PortAction {
    /// Toggle the state of DTR (Data Terminal Ready).
    ToggleDtr,
    /// Toggle the state of RTS (Ready To Send).
    ToggleRts,

    /// Set the state of RTS to active (true).
    AssertRts,
    /// Set the state of RTS to inactive (false).
    DeassertRts,

    /// Set the state of DTR to active (true).
    AssertDtr,
    /// Set the state of DTR to inactive (false).
    DeassertDtr,

    /// Attempt to reconnect to device, must match USB info if applicable.
    AttemptReconnectStrictHigh,
    AttemptReconnectStrictMed,
    /// Attempt to reconnect to device, best-effort.
    AttemptReconnectStrictLow,
}

impl RequiresPort for PortAction {
    fn requires_connection(&self) -> bool {
        match self {
            // Can't require a connection for the method to revitalize one.
            Self::AttemptReconnectStrictHigh
            | Self::AttemptReconnectStrictMed
            | Self::AttemptReconnectStrictLow => false,
            _ => true,
        }
    }
    fn requires_terminal_view(&self) -> bool {
        // All actions interact with a device.
        true
    }
}

#[cfg(feature = "macros")]
#[derive(
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
    strum::VariantArray,
    strum::EnumMessage,
)]
#[strum(serialize_all = "kebab-case")]
#[strum(ascii_case_insensitive)]
pub enum MacroBuiltinAction {
    /// Reload all Macros.
    ReloadMacros,
}

#[cfg(feature = "macros")]
impl RequiresPort for MacroBuiltinAction {
    fn requires_connection(&self) -> bool {
        false
    }
}

#[cfg(feature = "espflash")]
#[derive(
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
    strum::VariantArray,
    strum::EnumMessage,
)]
#[strum(serialize_all = "kebab-case")]
#[strum(ascii_case_insensitive)]
pub enum EspBuiltinAction {
    /// Attempt to remotely reset the chip.
    EspHardReset,
    /// Attempt to reboot into bootloader, retries until success or eventual fail.
    EspBootloader,
    /// Attempt to reboot into bootloader, doesn't check for success.
    EspBootloaderUnchecked,
    /// Query ESP for Flash Size, MAC Address, etc.
    EspDeviceInfo,
    /// Erase all ESP flash contents.
    EspEraseFlash,
    #[strum(serialize = "reload-espflash")]
    /// Reload all espflash profiles.
    ReloadProfiles,
}

#[cfg(feature = "espflash")]
impl RequiresPort for EspBuiltinAction {
    fn requires_connection(&self) -> bool {
        match self {
            // UI action, can happen whenever.
            Self::ReloadProfiles => false,
            // But the rest require a healthy connection.
            _ => true,
        }
    }
}

#[cfg(feature = "logging")]
#[derive(
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
    strum::VariantArray,
    strum::EnumMessage,
)]
#[strum(ascii_case_insensitive)]
pub enum LoggingAction {
    #[strum(serialize = "logging-sync")]
    /// Sync any active log files with entire buffer content, current settings, and defmt table if loaded.
    Sync,
}

#[cfg(feature = "logging")]
impl RequiresPort for LoggingAction {
    fn requires_connection(&self) -> bool {
        false
    }
    fn requires_terminal_view(&self) -> bool {
        match self {
            // No log file would be made without a connection,
            // and any existing files are flushed and dropped
            // when returning to port selection screen.
            Self::Sync => true,
            // _ => false,
        }
    }
}

#[cfg(feature = "defmt")]
#[derive(
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
    strum::VariantArray,
    strum::EnumMessage,
)]
#[strum(serialize_all = "kebab-case")]
#[strum(ascii_case_insensitive)]
#[repr(u8)]
/// Actions to show one of the defmt ELF selection screens.
pub enum DefmtSelectAction {
    #[strum(serialize = "defmt-select-tui")]
    /// Select a defmt ELF with an in-app file explorer.
    SelectTui,
    #[strum(serialize = "defmt-select-system")]
    /// Select a defmt ELF with the system file explorer.
    SelectSystem,
    #[strum(serialize = "defmt-select-recent")]
    /// Select a recently-used defmt ELF.
    SelectRecent,
}

#[cfg(feature = "defmt")]
impl RequiresPort for DefmtSelectAction {
    fn requires_connection(&self) -> bool {
        // "UI" only.
        false
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, strum::AsRefStr)]
pub enum BuiltinAction {
    Base(BaseAction),
    Popup(ShowPopupAction),
    Port(PortAction),
    #[cfg(feature = "macros")]
    MacroBuiltin(MacroBuiltinAction),
    #[cfg(feature = "espflash")]
    EspBuiltin(EspBuiltinAction),
    #[cfg(feature = "logging")]
    Logging(LoggingAction),
    #[cfg(feature = "defmt")]
    ShowDefmtSelect(DefmtSelectAction),
}

impl RequiresPort for BuiltinAction {
    fn requires_connection(&self) -> bool {
        match self {
            Self::Base(action) => action.requires_connection(),
            Self::Popup(action) => action.requires_connection(),
            Self::Port(action) => action.requires_connection(),
            #[cfg(feature = "macros")]
            Self::MacroBuiltin(action) => action.requires_connection(),
            #[cfg(feature = "espflash")]
            Self::EspBuiltin(action) => action.requires_connection(),
            #[cfg(feature = "logging")]
            Self::Logging(action) => action.requires_connection(),
            #[cfg(feature = "defmt")]
            Self::ShowDefmtSelect(action) => action.requires_connection(),
        }
    }
    fn requires_terminal_view(&self) -> bool {
        match self {
            Self::Base(action) => action.requires_terminal_view(),
            Self::Popup(action) => action.requires_terminal_view(),
            Self::Port(action) => action.requires_terminal_view(),
            #[cfg(feature = "macros")]
            Self::MacroBuiltin(action) => action.requires_terminal_view(),
            #[cfg(feature = "espflash")]
            Self::EspBuiltin(action) => action.requires_terminal_view(),
            #[cfg(feature = "logging")]
            Self::Logging(action) => action.requires_terminal_view(),
            #[cfg(feature = "defmt")]
            Self::ShowDefmtSelect(action) => action.requires_terminal_view(),
        }
    }
}

impl fmt::Display for BuiltinAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuiltinAction::Base(action) => write!(f, "{action}"),
            BuiltinAction::Popup(action) => write!(f, "{action}"),
            BuiltinAction::Port(action) => write!(f, "{action}"),
            #[cfg(feature = "macros")]
            BuiltinAction::MacroBuiltin(action) => write!(f, "{action}"),
            #[cfg(feature = "espflash")]
            BuiltinAction::EspBuiltin(action) => write!(f, "{action}"),
            #[cfg(feature = "logging")]
            BuiltinAction::Logging(action) => write!(f, "{action}"),
            #[cfg(feature = "defmt")]
            BuiltinAction::ShowDefmtSelect(action) => write!(f, "{action}"),
        }
    }
}

impl From<BaseAction> for BuiltinAction {
    fn from(action: BaseAction) -> Self {
        BuiltinAction::Base(action)
    }
}

impl From<ShowPopupAction> for BuiltinAction {
    fn from(action: ShowPopupAction) -> Self {
        BuiltinAction::Popup(action)
    }
}

impl From<PortAction> for BuiltinAction {
    fn from(action: PortAction) -> Self {
        BuiltinAction::Port(action)
    }
}

#[cfg(feature = "macros")]
impl From<MacroBuiltinAction> for BuiltinAction {
    fn from(action: MacroBuiltinAction) -> Self {
        BuiltinAction::MacroBuiltin(action)
    }
}

#[cfg(feature = "espflash")]
impl From<EspBuiltinAction> for BuiltinAction {
    fn from(action: EspBuiltinAction) -> Self {
        BuiltinAction::EspBuiltin(action)
    }
}

#[cfg(feature = "logging")]
impl From<LoggingAction> for BuiltinAction {
    fn from(action: LoggingAction) -> Self {
        BuiltinAction::Logging(action)
    }
}

#[cfg(feature = "defmt")]
impl From<DefmtSelectAction> for BuiltinAction {
    fn from(action: DefmtSelectAction) -> Self {
        BuiltinAction::ShowDefmtSelect(action)
    }
}

impl FromStr for BuiltinAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Ok(base) = s.parse::<BaseAction>() {
            return Ok(BuiltinAction::Base(base));
        }
        if let Ok(popup) = s.parse::<ShowPopupAction>() {
            return Ok(BuiltinAction::Popup(popup));
        }
        if let Ok(port) = s.parse::<PortAction>() {
            return Ok(BuiltinAction::Port(port));
        }
        #[cfg(feature = "logging")]
        if let Ok(logging) = s.parse::<LoggingAction>() {
            return Ok(BuiltinAction::Logging(logging));
        }
        #[cfg(feature = "macros")]
        if let Ok(macros) = s.parse::<MacroBuiltinAction>() {
            return Ok(BuiltinAction::MacroBuiltin(macros));
        }
        #[cfg(feature = "espflash")]
        if let Ok(esp) = s.parse::<EspBuiltinAction>() {
            return Ok(BuiltinAction::EspBuiltin(esp));
        }
        #[cfg(feature = "defmt")]
        if let Ok(defmt_select) = s.parse::<DefmtSelectAction>() {
            return Ok(BuiltinAction::ShowDefmtSelect(defmt_select));
        }

        Err(format!(
            "Unrecognized BuiltinAction variant for string: {s}"
        ))
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    BuiltinAction(BuiltinAction),
    #[cfg(feature = "espflash")]
    EspFlashProfile(String),
    #[cfg(feature = "macros")]
    MacroInvocation(MacroNameTag),
    Pause(Duration),
}

impl RequiresPort for Action {
    fn requires_connection(&self) -> bool {
        match self {
            Self::BuiltinAction(action) => action.requires_connection(),
            #[cfg(feature = "espflash")]
            // Flashing binaries to a port requires a healthy connection.
            Self::EspFlashProfile(_) => true,
            #[cfg(feature = "macros")]
            // Sending contents to a port requires a healthy connection.
            Self::MacroInvocation(_) => true,
            // Sitting around and doing nothing does not.
            Self::Pause(_) => false,
        }
    }
    fn requires_terminal_view(&self) -> bool {
        match self {
            Self::BuiltinAction(action) => action.requires_terminal_view(),
            _ => self.requires_connection(),
        }
    }
}

/// The default keybinds that are always loaded.
///
/// User's keybind file can override these keybinds without error.
static OVERRIDABLE_DEFAULTS: &str = r#"
[keybindings]
ctrl-o = "toggle-dtr"
ctrl-p = "toggle-rts"

ctrl-w = "toggle-textwrap"
ctrl-y = "toggle-timestamps"
ctrl-d = "toggle-indices"

ctrl-b = "toggle-pseudo-shell-hex"
'ctrl-.' = "show-portsettings"

ctrl-f = "reload-colors"

ctrl-t = "escape-keypress"

ctrl-h = "show-keybinds"
ctrl-k = "show-keybinds"
'ctrl-/' = "show-keybinds"
"#;

pub const CONFIG_TOML_PATH: &str = "yap_keybinds.toml";

/// If the user does not have an Escape Keypress action bound,
/// this will override anything they have in Ctrl-t to always allow
/// escaping the direct keyboard input mode, preventing someone from getting trapped.
const DEFAULT_KEYPRESS_ESCAPE: KeyCombination =
    KeyCombination::one_key(KeyCode::Char('t'), KeyModifiers::CONTROL);

// TODO add a category for "text-based" actions?
// like history scrolling/word removal/cursor manipulation?

#[derive(Deserialize)]
pub struct Keybinds {
    #[serde(deserialize_with = "deserialize_keybinds_map")]
    #[serde(default)]
    pub keybindings: IndexMap<KeyCombination, Vec<String>>,

    // To avoid doing a search + to_string()
    // every single time we render these commonly shown hints.
    #[serde(skip)]
    port_settings_hint: Option<CompactString>,
    #[serde(skip)]
    show_keybinds_hint: Option<CompactString>,
    #[serde(skip)]
    escape_keypress_hint: CompactString,
}

// fn serialize_macros_map<S>(
//     map: &IndexMap<KeyCombination, Vec<MacroNameTag>>,
//     serializer: S,
// ) -> Result<S::Ok, S::Error>
// where
//     S: serde::Serializer,
// {
//     use serde::ser::SerializeMap;
//     let mut map_ser = serializer.serialize_map(Some(map.len()))?;
//     for (k, v) in map {
//         let joined = v
//             .iter()
//             .map(|tag| tag.to_serialized_format())
//             .collect::<Vec<_>>()
//             .join(MACRO_CHAIN_DELIMITER);
//         map_ser.serialize_entry(k, &joined)?;
//     }
//     map_ser.end()
// }

fn deserialize_keybinds_map<'de, D>(
    deserializer: D,
) -> Result<IndexMap<KeyCombination, Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{MapAccess, Visitor};
    use std::fmt;

    struct KeybindsMapVisitor;

    impl<'de> Visitor<'de> for KeybindsMapVisitor {
        type Value = IndexMap<KeyCombination, Vec<String>>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a map from key combination to delimited keybind actions")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut result = IndexMap::new();

            // Don't really need this type anywhere else, so it's fine being super private.
            // Also serde_as already has something for this apparently.
            #[derive(serde::Deserialize)]
            #[serde(untagged)]
            enum SingleOrSeveral<T> {
                Single(String),
                Several(Vec<T>),
            }

            while let Some((key, value)) =
                map.next_entry::<KeyCombination, SingleOrSeveral<String>>()?
            {
                let mut tags = match value {
                    SingleOrSeveral::Single(value) if value.trim().is_empty() => continue,
                    SingleOrSeveral::Single(single) => vec![single],
                    SingleOrSeveral::Several(pre_split) => pre_split,
                };

                // Remove any empty chain entries
                tags.retain(|tag| !tag.trim().is_empty());

                // And just don't bother remembering empty bindings.
                if tags.is_empty() {
                    continue;
                }

                result.insert(key, tags);
            }

            Ok(result)
        }
    }

    deserializer.deserialize_map(KeybindsMapVisitor)
}

impl Keybinds {
    /// Load keybinds from disk.
    ///
    /// If not present, creates a file and fills with example contents.
    pub fn load() -> Result<Self, KeybindLoadError> {
        let keybinds_path = config_adjacent_path(crate::keybinds::CONFIG_TOML_PATH);
        let keybinds = if keybinds_path.exists() {
            let keybinds_str =
                fs::read_to_string(keybinds_path).map_err(KeybindLoadError::FileRead)?;
            Keybinds::from_str(&keybinds_str)?
        } else {
            fs::write(
                keybinds_path,
                include_str!("../example_configs/yap_keybinds.toml.blank").as_bytes(),
            )
            .map_err(KeybindLoadError::FileWrite)?;
            Keybinds::overridable_defaults()
        };
        Ok(keybinds)
    }
    fn fill_hints(&mut self) {
        // We require this to be bound since otherwise the user can get themselves stuck.
        // Ideally this never overrides a user's action, but c'est la vie.
        if self
            .find_key_with_single_action(BaseAction::EscapeKeypress.into())
            .is_none()
        {
            self.keybindings.insert(
                DEFAULT_KEYPRESS_ESCAPE,
                vec![BaseAction::EscapeKeypress.to_string()],
            );
        }

        self.port_settings_hint = self
            .find_key_with_single_action(ShowPopupAction::ShowPortSettings.into())
            .map(|kc| kc.to_compact_string());

        self.show_keybinds_hint = self
            .find_key_with_single_action(ShowPopupAction::ShowKeybinds.into())
            .map(|kc| kc.to_compact_string());

        self.escape_keypress_hint = self
            .find_key_with_single_action(BaseAction::EscapeKeypress.into())
            .map(|kc| kc.to_compact_string())
            .expect("This action must be bound, and should've been forcibly bound on load.");
    }
    pub fn port_settings_hint(&self) -> &str {
        self.port_settings_hint
            .as_ref()
            .map(CompactString::as_str)
            .unwrap_or_else(|| "UNBOUND")
    }
    pub fn show_keybinds_hint(&self) -> &str {
        self.show_keybinds_hint
            .as_ref()
            .map(CompactString::as_str)
            .unwrap_or_else(|| "UNBOUND")
    }
    pub fn escape_keypress_hint(&self) -> &str {
        self.escape_keypress_hint.as_ref()
    }
    fn overridable_defaults() -> Self {
        let mut deserialized: Self =
            toml::from_str(OVERRIDABLE_DEFAULTS).expect("hardcoded default should be valid");

        deserialized.fill_hints();

        deserialized
    }
    fn from_str(input: &str) -> Result<Self, toml::de::Error> {
        let mut overridable = Self::overridable_defaults();

        let user_settings: Self = toml::from_str(input)?;

        if user_settings.keybindings.is_empty() {
            return Ok(overridable);
        }
        // Anything the user has supplied with the same key will be overwritten.
        overridable.keybindings.extend(user_settings.keybindings);

        overridable.fill_hints();

        Ok(overridable)
    }
    /// Returns the first keybind found with the supplied action as the only bound action.
    fn find_key_with_single_action(&self, action: BuiltinAction) -> Option<KeyCombination> {
        self.keybindings
            .iter()
            .find(|(_, actions)| {
                if actions.len() != 1 {
                    return false;
                }

                actions[0]
                    .parse::<BuiltinAction>()
                    .ok()
                    .is_some_and(|a| a == action)
            })
            .map(|(key_combo, _)| *key_combo)
    }
    pub fn key_has_single_action(&self, key_combo: KeyCombination, action: BuiltinAction) -> bool {
        self.keybindings
            .get(&key_combo)
            .and_then(|actions| {
                if actions.len() == 1 {
                    actions[0].parse().ok()
                } else {
                    None
                }
            })
            .is_some_and(|parsed_action| action == parsed_action)
    }
    pub fn action_strs_from_key_combo(&self, key_combo: KeyCombination) -> Option<&Vec<String>> {
        self.keybindings.get(&key_combo)
    }
}

// #[cfg(test)]
// mod test {
//     use super::*;

//     #[test]
//     fn test_default_config_deser() {
//         let keybinds = Keybinds::from_str(include_str!("../example_configs/yap_keybinds.toml"))
//             .expect("example configs should be valid");
//         assert!(!keybinds.keybindings.is_empty());

//         let port_settings_bind = keybinds.keybindings.get(&crokey::key!(ctrl - '.')).unwrap();
//         assert_eq!(port_settings_bind[0], "show-portsettings");
//         assert_eq!(
//             keybinds
//                 .port_settings_hint
//                 .as_ref()
//                 .map(CompactString::as_str),
//             Some("Ctrl-.")
//         );
//     }
// }

pub fn print_all_actions() {
    use ratatui::crossterm::style::Stylize;
    fn print_variants<T: VariantArray + EnumMessage + fmt::Display>(name: &str) {
        use ratatui::crossterm::style::Stylize;

        let text = format!("{name}:").red();
        println!("{text}");
        for variant in T::VARIANTS {
            let mut doc_comment = String::from(" - ");
            doc_comment.push_str(
                variant
                    .get_documentation()
                    .unwrap_or_else(|| panic!("BuiltinAction {variant} missing doc comment")),
            );
            let styled_doc = doc_comment.dark_grey();

            println!("{variant}{styled_doc}");
        }
        println!();
    }

    print_variants::<ShowPopupAction>("Show Popup Actions");
    print_variants::<BaseAction>("Base Actions");
    print_variants::<PortAction>("Port Actions");

    #[cfg(feature = "macros")]
    print_variants::<MacroBuiltinAction>("Macro Actions");

    #[cfg(feature = "espflash")]
    print_variants::<EspBuiltinAction>("ESP Actions");

    #[cfg(feature = "logging")]
    print_variants::<LoggingAction>("Logging Actions");

    #[cfg(feature = "defmt")]
    print_variants::<DefmtSelectAction>("defmt Selection Actions");

    let tip = "Tip:".green();

    #[cfg(feature = "espflash")]
    {
        println!(
            "\n{tip} espflash profiles can be used as Actions! Invoke a profile to be flashed to the connected device by specifying the full exact name of the profile in your keybind."
        );
        let key = "Ctrl-Shift-N".cyan();
        let profile = "\"OpenShock Core V2 1.4.0\"".green();
        println!("\nExample: {key} = {profile}");
        println!("\n")
    }

    #[cfg(feature = "macros")]
    {
        println!(
            "\n{tip} Macros can be used as Actions! You can invoke a macro by specifying the category and name, delimiting with the pipe (|) character."
        );
        let macro_example = "OpenShock|Factory Reset".cyan();
        println!("\nExample: {macro_example}");
        println!(
            "\nIf you often will not have colliding Macro names, you can skip the need to specify category by enabling `fuzzy_macro_match` in yap.toml"
        );
        println!(
            "You can still specify a category when `fuzzy_macro_match` is enabled in case a name collision does occur."
        );
        println!("\n")
    }

    println!(
        "\n{tip} You can chain Actions together in sequence! In `yap_keybinds.toml` when defining a key, use an array to specify multiple Actions!"
    );
    // silly
    if cfg!(feature = "macros") && cfg!(feature = "espflash") {
        println!("This includes Macros and espflash profiles!");
    } else if cfg!(feature = "macros") {
        println!("This includes Macros!");
    } else if cfg!(feature = "espflash") {
        println!("This includes espflash profiles!");
    }

    let f18 = "F18".cyan();
    let array = "[\"assert-rts\", \"assert-dtr\", \"deassert-rts\"]".red();
    println!("\nExample: {f18} = {array}");
    let pause = "PAUSE_MS:[milliseconds to wait]".cyan();
    println!(
        "\n\nA custom delay can be set between actions using {pause}. This will always take precedence over yap.toml's `action_chain_delay`."
    );
}

#[derive(Debug, thiserror::Error)]
pub enum KeybindLoadError {
    #[error("failed reading from keybinds file")]
    FileRead(#[source] std::io::Error),
    #[error("failed saving to keybinds file")]
    FileWrite(#[source] std::io::Error),
    #[error("invalid keybinds file")]
    Deser(#[from] toml::de::Error),
}
