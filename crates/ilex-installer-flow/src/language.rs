//! The installed system's language. The interface itself is English in the
//! first version (every string is still marked for translation); this is the
//! locale the installed system starts with (`LANG`), written by the helper
//! through `systemd-firstboot`.
//!
//! The list is a short curated table of UTF-8 locales: the live image has no
//! list of the locales the installed system can generate, so the model only
//! checks the syntax of a locale name and a well-formed name the system lacks
//! falls back to the C locale there. A person who needs another writes an
//! answers file.

/// A locale the installed system can start in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Language {
    /// `de_DE.UTF-8`
    pub locale: &'static str,
    /// The name in English: `German (Germany)`.
    pub english: &'static str,
    /// The language's own name, so that a person who does not read English
    /// finds it: `Deutsch`.
    pub native: &'static str,
}

impl Language {
    /// What the list shows.
    pub fn label(&self) -> String {
        if self.native == self.english.split(" (").next().unwrap_or(self.english) {
            self.english.to_owned()
        } else {
            format!("{} · {}", self.native, self.english)
        }
    }
}

pub const DEFAULT: &str = "en_US.UTF-8";

const fn l(locale: &'static str, english: &'static str, native: &'static str) -> Language {
    Language {
        locale,
        english,
        native,
    }
}

/// In the order of their English names.
pub const LANGUAGES: &[Language] = &[
    l("ar_EG.UTF-8", "Arabic (Egypt)", "العربية"),
    l("be_BY.UTF-8", "Belarusian (Belarus)", "Беларуская"),
    l("bg_BG.UTF-8", "Bulgarian (Bulgaria)", "Български"),
    l("ca_ES.UTF-8", "Catalan (Spain)", "Català"),
    l("zh_CN.UTF-8", "Chinese, Simplified (China)", "中文"),
    l("zh_TW.UTF-8", "Chinese, Traditional (Taiwan)", "中文"),
    l("hr_HR.UTF-8", "Croatian (Croatia)", "Hrvatski"),
    l("cs_CZ.UTF-8", "Czech (Czechia)", "Čeština"),
    l("da_DK.UTF-8", "Danish (Denmark)", "Dansk"),
    l("nl_BE.UTF-8", "Dutch (Belgium)", "Nederlands"),
    l("nl_NL.UTF-8", "Dutch (Netherlands)", "Nederlands"),
    l("en_AU.UTF-8", "English (Australia)", "English"),
    l("en_CA.UTF-8", "English (Canada)", "English"),
    l("en_IE.UTF-8", "English (Ireland)", "English"),
    l("en_GB.UTF-8", "English (United Kingdom)", "English"),
    l("en_US.UTF-8", "English (United States)", "English"),
    l("et_EE.UTF-8", "Estonian (Estonia)", "Eesti"),
    l("fi_FI.UTF-8", "Finnish (Finland)", "Suomi"),
    l("fr_BE.UTF-8", "French (Belgium)", "Français"),
    l("fr_CA.UTF-8", "French (Canada)", "Français"),
    l("fr_FR.UTF-8", "French (France)", "Français"),
    l("fr_CH.UTF-8", "French (Switzerland)", "Français"),
    l("de_AT.UTF-8", "German (Austria)", "Deutsch"),
    l("de_DE.UTF-8", "German (Germany)", "Deutsch"),
    l("de_CH.UTF-8", "German (Switzerland)", "Deutsch"),
    l("el_GR.UTF-8", "Greek (Greece)", "Ελληνικά"),
    l("he_IL.UTF-8", "Hebrew (Israel)", "עברית"),
    l("hi_IN.UTF-8", "Hindi (India)", "हिन्दी"),
    l("hu_HU.UTF-8", "Hungarian (Hungary)", "Magyar"),
    l("is_IS.UTF-8", "Icelandic (Iceland)", "Íslenska"),
    l("id_ID.UTF-8", "Indonesian (Indonesia)", "Bahasa Indonesia"),
    l("it_IT.UTF-8", "Italian (Italy)", "Italiano"),
    l("ja_JP.UTF-8", "Japanese (Japan)", "日本語"),
    l("ko_KR.UTF-8", "Korean (South Korea)", "한국어"),
    l("lv_LV.UTF-8", "Latvian (Latvia)", "Latviešu"),
    l("lt_LT.UTF-8", "Lithuanian (Lithuania)", "Lietuvių"),
    l("nb_NO.UTF-8", "Norwegian Bokmål (Norway)", "Norsk bokmål"),
    l("fa_IR.UTF-8", "Persian (Iran)", "فارسی"),
    l("pl_PL.UTF-8", "Polish (Poland)", "Polski"),
    l("pt_BR.UTF-8", "Portuguese (Brazil)", "Português"),
    l("pt_PT.UTF-8", "Portuguese (Portugal)", "Português"),
    l("ro_RO.UTF-8", "Romanian (Romania)", "Română"),
    l("ru_RU.UTF-8", "Russian (Russia)", "Русский"),
    l("sr_RS.UTF-8", "Serbian (Serbia)", "Српски"),
    l("sk_SK.UTF-8", "Slovak (Slovakia)", "Slovenčina"),
    l("sl_SI.UTF-8", "Slovenian (Slovenia)", "Slovenščina"),
    l("es_AR.UTF-8", "Spanish (Argentina)", "Español"),
    l("es_MX.UTF-8", "Spanish (Mexico)", "Español"),
    l("es_ES.UTF-8", "Spanish (Spain)", "Español"),
    l("sv_SE.UTF-8", "Swedish (Sweden)", "Svenska"),
    l("th_TH.UTF-8", "Thai (Thailand)", "ไทย"),
    l("tr_TR.UTF-8", "Turkish (Türkiye)", "Türkçe"),
    l("uk_UA.UTF-8", "Ukrainian (Ukraine)", "Українська"),
    l("vi_VN.UTF-8", "Vietnamese (Vietnam)", "Tiếng Việt"),
];

pub fn find(locale: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|l| l.locale == locale)
}

pub fn index_of(locale: &str) -> Option<usize> {
    LANGUAGES.iter().position(|l| l.locale == locale)
}
