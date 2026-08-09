use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionRuleClass {
    Hard,
    Default,
}

impl ExclusionRuleClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Default => "default",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "hard" => Self::Hard,
            _ => Self::Default,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionRuleType {
    ExactPath,
    PathName,
    PathGlob,
    Extension,
    Hidden,
    System,
    ReparsePoint,
    CloudPlaceholder,
}

impl ExclusionRuleType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactPath => "exact_path",
            Self::PathName => "path_name",
            Self::PathGlob => "path_glob",
            Self::Extension => "extension",
            Self::Hidden => "hidden",
            Self::System => "system",
            Self::ReparsePoint => "reparse_point",
            Self::CloudPlaceholder => "cloud_placeholder",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "exact_path" => Self::ExactPath,
            "path_glob" => Self::PathGlob,
            "extension" => Self::Extension,
            "hidden" => Self::Hidden,
            "system" => Self::System,
            "reparse_point" => Self::ReparsePoint,
            "cloud_placeholder" => Self::CloudPlaceholder,
            _ => Self::PathName,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExclusionRule {
    pub rule_id: Uuid,
    pub root_id: Option<Uuid>,
    pub rule_class: ExclusionRuleClass,
    pub rule_type: ExclusionRuleType,
    pub value: Value,
    pub enabled: bool,
    pub overridable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltInExclusionRule {
    pub key: &'static str,
    pub rule_class: ExclusionRuleClass,
    pub rule_type: ExclusionRuleType,
    pub value: &'static str,
    pub overridable: bool,
}

pub const EXCLUSION_RULES_VERSION: u32 = 1;

pub const BUILT_IN_EXCLUSION_RULES: &[BuiltInExclusionRule] = &[
    BuiltInExclusionRule {
        key: "hard-recycle-bin",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::PathName,
        value: "$recycle.bin",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-system-volume",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::PathName,
        value: "system volume information",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-windows",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::PathName,
        value: "windows",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-program-files",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::PathName,
        value: "program files",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-program-files-x86",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::PathName,
        value: "program files (x86)",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-program-data",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::PathName,
        value: "programdata",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-app-data",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::PathName,
        value: "appdata",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-ssh",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::PathName,
        value: ".ssh",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-gnupg",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::PathName,
        value: ".gnupg",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-hidden",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::Hidden,
        value: "true",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-system",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::System,
        value: "true",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-reparse",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::ReparsePoint,
        value: "true",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "hard-cloud-placeholder",
        rule_class: ExclusionRuleClass::Hard,
        rule_type: ExclusionRuleType::CloudPlaceholder,
        value: "true",
        overridable: false,
    },
    BuiltInExclusionRule {
        key: "default-git",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: ".git",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-svn",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: ".svn",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-hg",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: ".hg",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-node-modules",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: "node_modules",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-venv-dot",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: ".venv",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-venv",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: "venv",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-pycache",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: "__pycache__",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-build",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: "build",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-dist",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: "dist",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-target",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: "target",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-cache",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathName,
        value: ".cache",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-tmp",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::Extension,
        value: "tmp",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-part",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::Extension,
        value: "part",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-crdownload",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::Extension,
        value: "crdownload",
        overridable: true,
    },
    BuiltInExclusionRule {
        key: "default-office-lock",
        rule_class: ExclusionRuleClass::Default,
        rule_type: ExclusionRuleType::PathGlob,
        value: "~$*",
        overridable: true,
    },
];
