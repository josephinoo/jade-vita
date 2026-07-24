#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Locale {
    #[default]
    EnUs,
    EsEs,
}

impl Locale {
    pub const ALL: [Locale; 2] = [Self::EnUs, Self::EsEs];

    /// `(locale code, store market, native-language label)`.
    fn info(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::EnUs => ("en-US", "US", "English (US)"),
            Self::EsEs => ("es-ES", "ES", "Español (España)"),
        }
    }

    /// The locale code sent to xCloud, e.g. `"es-ES"`.
    pub fn as_str(self) -> &'static str {
        self.info().0
    }

    /// Native-language label shown in the language picker, e.g. `"Español (España)"`.
    pub fn label(self) -> &'static str {
        self.info().2
    }
}
