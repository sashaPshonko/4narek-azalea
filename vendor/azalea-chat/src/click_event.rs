use serde::Serialize;
use serde_json::Value;
#[cfg(feature = "simdnbt")]
use simdnbt::owned::{Nbt, NbtCompound, NbtTag};

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum ClickEvent {
    OpenUrl {
        url: String,
    },
    OpenFile {
        path: String,
    },
    RunCommand {
        command: String,
    },
    SuggestCommand {
        command: String,
    },
    // TODO: this uses Dialog.CODEC
    ShowDialog,
    ChangePage {
        page: i32,
    },
    CopyToClipboard {
        value: String,
    },
    Custom {
        id: String,
        #[cfg(feature = "simdnbt")]
        payload: Nbt,
    },
}

impl ClickEvent {
    /// JSON `click_event` / legacy `clickEvent` (run_command / suggest_command).
    pub fn from_json(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let action = obj.get("action")?.as_str()?.to_ascii_lowercase();
        let cmd = obj
            .get("command")
            .or_else(|| obj.get("value"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_owned());
        match action.as_str() {
            "run_command" => Some(Self::RunCommand {
                command: cmd.unwrap_or_default(),
            }),
            "suggest_command" => Some(Self::SuggestCommand {
                command: cmd.unwrap_or_default(),
            }),
            "open_url" | "openurl" => Some(Self::OpenUrl {
                url: obj
                    .get("url")
                    .or_else(|| obj.get("value"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_owned(),
            }),
            "copy_to_clipboard" | "copytoclipboard" => Some(Self::CopyToClipboard {
                value: obj
                    .get("value")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_owned(),
            }),
            "change_page" | "changepage" => Some(Self::ChangePage {
                page: obj
                    .get("page")
                    .or_else(|| obj.get("value"))
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0) as i32,
            }),
            "open_file" | "openfile" => Some(Self::OpenFile {
                path: obj
                    .get("path")
                    .or_else(|| obj.get("value"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_owned(),
            }),
            "show_dialog" => Some(Self::ShowDialog),
            _ => None,
        }
    }

    #[cfg(feature = "simdnbt")]
    pub fn from_nbt(compound: simdnbt::borrow::NbtCompound) -> Option<Self> {
        let action = compound.string("action")?.to_string().to_ascii_lowercase();
        let cmd = compound
            .string("command")
            .or_else(|| compound.string("value"))
            .map(|s| s.to_string());
        match action.as_str() {
            "run_command" => Some(Self::RunCommand {
                command: cmd.unwrap_or_default(),
            }),
            "suggest_command" => Some(Self::SuggestCommand {
                command: cmd.unwrap_or_default(),
            }),
            "open_url" => Some(Self::OpenUrl {
                url: compound
                    .string("url")
                    .or_else(|| compound.string("value"))
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            }),
            "copy_to_clipboard" => Some(Self::CopyToClipboard {
                value: compound
                    .string("value")
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            }),
            "change_page" => Some(Self::ChangePage {
                page: compound.int("page").unwrap_or(0),
            }),
            "open_file" => Some(Self::OpenFile {
                path: compound
                    .string("path")
                    .or_else(|| compound.string("value"))
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            }),
            "show_dialog" => Some(Self::ShowDialog),
            _ => None,
        }
    }
}

#[cfg(feature = "simdnbt")]
impl simdnbt::Serialize for ClickEvent {
    fn to_compound(self) -> NbtCompound {
        let mut compound = NbtCompound::new();
        let mut action = |s: &str| {
            compound.insert("action", s);
        };
        match self {
            ClickEvent::OpenUrl { url } => {
                action("open_url");
                compound.insert("url", url);
            }
            ClickEvent::OpenFile { path } => {
                action("open_file");
                compound.insert("path", path);
            }
            ClickEvent::RunCommand { command } => {
                action("run_command");
                compound.insert("command", command);
            }
            ClickEvent::SuggestCommand { command } => {
                action("suggest_command");
                compound.insert("command", command);
            }
            ClickEvent::ShowDialog => {
                action("show_dialog");
            }
            ClickEvent::ChangePage { page } => {
                action("change_page");
                compound.insert("page", NbtTag::Int(page));
            }
            ClickEvent::CopyToClipboard { value } => {
                action("copy_to_clipboard");
                compound.insert("value", value);
            }
            ClickEvent::Custom { id, payload } => {
                action("custom");
                compound.insert("id", id);
                compound.insert("payload", (**payload).clone());
            }
        }
        compound
    }
}
