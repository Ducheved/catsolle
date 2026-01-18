use anyhow::Result;
use fluent_bundle::FluentArgs;
use i18n_embed::fluent::{fluent_language_loader, FluentLanguageLoader};
use i18n_embed::DesktopLanguageRequester;
use rust_embed::RustEmbed;
use unic_langid::LanguageIdentifier;

#[derive(Debug, thiserror::Error)]
pub enum I18nError {
    #[error("i18n error: {0}")]
    I18n(#[from] i18n_embed::I18nEmbedError),
    #[error("lang parse error: {0}")]
    Lang(#[from] unic_langid::LanguageIdentifierError),
}

#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Localizations;

pub struct I18n {
    loader: FluentLanguageLoader,
    language: LanguageIdentifier,
}

impl I18n {
    pub fn new(default_lang: &str, preferred: &[String]) -> Result<Self, I18nError> {
        let loader = fluent_language_loader!();
        let mut requested = DesktopLanguageRequester::requested_languages();
        for lang in preferred {
            if let Ok(id) = lang.parse::<LanguageIdentifier>() {
                requested.push(id);
            }
        }
        let default_id: LanguageIdentifier = default_lang.parse()?;
        let selected = i18n_embed::select(&loader, &Localizations, &requested).or_else(|_| {
            i18n_embed::select(&loader, &Localizations, std::slice::from_ref(&default_id))
        })?;
        let language = selected.first().cloned().unwrap_or(default_id);
        Ok(Self { loader, language })
    }

    pub fn language(&self) -> &LanguageIdentifier {
        &self.language
    }

    pub fn tr(&self, key: &str) -> String {
        self.loader.get(key)
    }

    pub fn tr_args(&self, key: &str, args: &FluentArgs) -> String {
        self.loader.get_args_fluent(key, Some(args))
    }
}
