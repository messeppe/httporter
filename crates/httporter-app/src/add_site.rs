//! The Add Site form: what the sheet collects and the checks made before a connection is tried.
//!
//! The sheet itself only gathers text; it hands the four values to the app through
//! `add-site-test` and `add-site-save`, and the app calls [`AddSiteForm::validate`]. A failure
//! comes back as one [`FieldError`], which [`show_field_error`] puts under the field it names.
//!
//! The password passes through here but is never logged, printed or stored: the `Debug` output of
//! the form types leaves it out.

use std::fmt;

use slint::SharedString;

use crate::AppWindow;

/// What the user typed, exactly as the sheet reports it.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct AddSiteForm {
    /// Site name; empty means "same as the zone".
    pub name: String,
    pub zone: String,
    /// Index into the region list the sheet shows (negative when nothing is chosen).
    pub region_index: i32,
    pub password: String,
}

/// A form that passed [`AddSiteForm::validate`]: trimmed, with the region as its code.
#[derive(Clone, PartialEq, Eq)]
pub struct ValidSite {
    pub name: String,
    pub zone: String,
    pub region_code: String,
    pub password: String,
}

/// The first problem found, with the text to show under the field it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldError {
    Zone(String),
    Region(String),
    Password(String),
}

impl FieldError {
    /// The text to show under the field.
    pub fn message(&self) -> &str {
        match self {
            FieldError::Zone(m) | FieldError::Region(m) | FieldError::Password(m) => m,
        }
    }
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

// Not derived: a derived `Debug` would print the password in a failed assertion or a log line.
impl fmt::Debug for AddSiteForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AddSiteForm")
            .field("name", &self.name)
            .field("zone", &self.zone)
            .field("region_index", &self.region_index)
            .field("password", &"<hidden>")
            .finish()
    }
}

impl fmt::Debug for ValidSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidSite")
            .field("name", &self.name)
            .field("zone", &self.zone)
            .field("region_code", &self.region_code)
            .field("password", &"<hidden>")
            .finish()
    }
}

impl AddSiteForm {
    /// Checks the form. `region_codes` are the codes behind the sheet's region list, in the same
    /// order. The first problem found is returned: zone, then region, then password.
    ///
    /// The zone and the name are trimmed, and a blank name becomes the zone. The password is kept
    /// exactly as typed.
    pub fn validate(&self, region_codes: &[&str]) -> Result<ValidSite, FieldError> {
        let zone = self.zone.trim();
        if zone.is_empty() {
            return Err(FieldError::Zone("Enter the storage zone name.".into()));
        }
        if zone.chars().any(char::is_whitespace) {
            return Err(FieldError::Zone("Zone names can't contain spaces.".into()));
        }
        let region_code = usize::try_from(self.region_index)
            .ok()
            .and_then(|index| region_codes.get(index))
            .ok_or_else(|| FieldError::Region("Choose a region.".into()))?;
        if self.password.is_empty() {
            return Err(FieldError::Password("Enter the zone's password.".into()));
        }
        let name = self.name.trim();
        Ok(ValidSite {
            name: if name.is_empty() { zone } else { name }.into(),
            zone: zone.into(),
            region_code: (*region_code).into(),
            password: self.password.clone(),
        })
    }
}

/// Shows `error` under its field on the open sheet and clears the other two; `None` clears all.
/// Showing an error also puts the keyboard focus in that field.
pub fn show_field_error(ui: &AppWindow, error: Option<&FieldError>) {
    let message_if = |wanted: bool| -> SharedString {
        match error {
            Some(error) if wanted => error.message().into(),
            _ => SharedString::new(),
        }
    };
    ui.set_add_site_zone_error(message_if(matches!(error, Some(FieldError::Zone(_)))));
    ui.set_add_site_region_error(message_if(matches!(error, Some(FieldError::Region(_)))));
    ui.set_add_site_password_error(message_if(matches!(error, Some(FieldError::Password(_)))));
    if error.is_some() {
        // Every error moves the keyboard focus to its field, also a repeat of the same one.
        ui.set_add_site_error_serial(ui.get_add_site_error_serial().wrapping_add(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REG: &[&str] = &["de", "sg"];

    fn form(zone: &str, region: i32, password: &str) -> AddSiteForm {
        AddSiteForm {
            name: String::new(),
            zone: zone.into(),
            region_index: region,
            password: password.into(),
        }
    }

    #[test]
    fn empty_zone() {
        assert_eq!(
            form("", 1, "pw").validate(REG),
            Err(FieldError::Zone("Enter the storage zone name.".into()))
        );
    }

    #[test]
    fn zone_with_spaces() {
        assert_eq!(
            form("my zone", 1, "pw").validate(REG),
            Err(FieldError::Zone("Zone names can't contain spaces.".into()))
        );
    }

    #[test]
    fn bad_region() {
        assert_eq!(
            form("z", 5, "pw").validate(REG),
            Err(FieldError::Region("Choose a region.".into()))
        );
    }

    #[test]
    fn empty_password() {
        assert_eq!(
            form("z", 1, "").validate(REG),
            Err(FieldError::Password("Enter the zone's password.".into()))
        );
    }

    #[test]
    fn a_typed_name_is_kept_and_trimmed() {
        let v = AddSiteForm {
            name: "  example-shop.com  ".into(),
            ..form("exampleshop", 0, "pw")
        }
        .validate(REG)
        .unwrap();
        assert_eq!(
            (v.name.as_str(), v.region_code.as_str()),
            ("example-shop.com", "de")
        );
    }

    #[test]
    fn the_first_problem_wins_in_field_order() {
        assert!(matches!(
            form("", 9, "").validate(REG),
            Err(FieldError::Zone(_))
        ));
        assert!(matches!(
            form("z", 9, "").validate(REG),
            Err(FieldError::Region(_))
        ));
        assert!(matches!(
            form("z", 0, "").validate(REG),
            Err(FieldError::Password(_))
        ));
    }

    #[test]
    fn no_region_chosen_and_an_empty_region_list_are_region_errors() {
        let choose = Err(FieldError::Region("Choose a region.".into()));
        assert_eq!(form("z", -1, "pw").validate(REG), choose);
        assert_eq!(form("z", 0, "pw").validate(&[]), choose);
        assert_eq!(form("z", 2, "pw").validate(REG), choose);
    }

    #[test]
    fn any_whitespace_inside_the_zone_counts_as_a_space() {
        for zone in ["my\tzone", "my\u{a0}zone", "a b"] {
            assert!(
                matches!(
                    form(zone, 0, "pw").validate(REG),
                    Err(FieldError::Zone(m)) if m.contains("spaces")
                ),
                "{zone:?}"
            );
        }
        // Whitespace around the zone is trimmed, not an error.
        assert_eq!(form("\tz \n", 0, "pw").validate(REG).unwrap().zone, "z");
    }

    #[test]
    fn a_blank_zone_is_empty_not_spaces() {
        let enter = Err(FieldError::Zone("Enter the storage zone name.".into()));
        assert_eq!(form("   ", 0, "pw").validate(REG), enter);
    }

    #[test]
    fn the_password_is_passed_on_unchanged() {
        let v = form("z", 0, " p w ").validate(REG).unwrap();
        assert_eq!(v.password, " p w ");
    }

    #[test]
    fn debug_output_never_contains_the_password() {
        let f = form("z", 0, "s3cret-pw");
        let v = f.validate(REG).unwrap();
        for text in [
            format!("{f:?}"),
            format!("{v:?}"),
            format!("{f:#?}"),
            format!("{v:#?}"),
        ] {
            assert!(!text.contains("s3cret-pw"), "{text}");
            assert!(text.contains("zone"), "{text}");
        }
    }

    #[test]
    fn name_defaults_to_zone_and_trims() {
        let v = AddSiteForm {
            name: "  ".into(),
            zone: " exampleshop ".into(),
            region_index: 1,
            password: "pw".into(),
        }
        .validate(REG)
        .unwrap();
        assert_eq!(
            (v.name.as_str(), v.zone.as_str(), v.region_code.as_str()),
            ("exampleshop", "exampleshop", "sg")
        );
    }
}
