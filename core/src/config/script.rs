const BASH_PREPROCESS: &str = r#"
    declare -A OPTIONS
    for key in "${!OPTION_@}"; do
        OPTIONS["${key#"OPTION_"}"]="${!key}"
    done
    unset OPTION_PREFIX
"#;

#[derive(Debug, Clone, Hash)]
pub enum ScriptLanguage {
    Bash,
    Python,
}

#[derive(Debug, Clone, Hash)]
pub struct Script {
    language: ScriptLanguage,
    text: String,
}

impl Script {
    pub fn new(language: ScriptLanguage, text: impl AsRef<str>) -> Self {
        Script {
            language,
            text: text.as_ref().to_string(),
        }
    }

    pub fn bash(text: impl AsRef<str>) -> Self {
        Script::new(ScriptLanguage::Bash, text)
    }

    pub fn command(&self) -> Vec<String> {
        match self.language {
            ScriptLanguage::Python => vec![String::from("python3"), String::from("-c"), self.text.clone()],
            ScriptLanguage::Bash => vec![
                String::from("bash"),
                String::from("-e"),
                String::from("-c"),
                format!("{}{}", BASH_PREPROCESS, self.text),
            ],
        }
    }
}
