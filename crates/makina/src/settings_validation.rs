//! Shared validation logic for SettingsCommit fields.
//!
//! This module provides a centralized validator for settings fields (gate_iterations,
//! reviewer_iterations, wall_clock_secs, idle_secs, concurrency) that is called
//! by both `App::update` and `event::commit_settings`. Each call site applies its
//! own error-surfacing strategy (early return vs. modal error field).

use crate::app::Settings;

/// Validated settings values extracted and type-converted from string input.
#[derive(Debug, Clone)]
pub struct SettingsValidation {
    pub gate_iterations: u32,
    pub reviewer_iterations: u32,
    pub wall_clock_secs: u64,
    pub idle_secs: Option<u64>,
    pub concurrency: usize,
}

/// Validates all settings fields and returns either a `SettingsValidation`
/// with type-converted values or an error message.
///
/// The error messages match exactly what both `App::update` and `event::commit_settings`
/// currently produce, ensuring consistent error-surfacing across both call sites.
pub fn validate_settings(settings: &Settings) -> Result<SettingsValidation, String> {
    // Parse gate_iterations
    let gate_iterations = match settings.gate_iterations.parse::<u32>() {
        Ok(val) => {
            if val >= 1 {
                val
            } else {
                return Err("caps.gate_iterations must be at least 1".to_string());
            }
        }
        Err(_) => {
            return Err("caps.gate_iterations must be a positive integer".to_string());
        }
    };

    // Parse reviewer_iterations
    let reviewer_iterations = match settings.reviewer_iterations.parse::<u32>() {
        Ok(val) => {
            if val >= 1 {
                val
            } else {
                return Err("caps.reviewer_iterations must be at least 1".to_string());
            }
        }
        Err(_) => {
            return Err("caps.reviewer_iterations must be a positive integer".to_string());
        }
    };

    // Parse wall_clock_secs
    let wall_clock_secs = match settings.wall_clock_secs.parse::<u64>() {
        Ok(val) => {
            if val >= 1 {
                val
            } else {
                return Err("caps.wall_clock_secs must be at least 1".to_string());
            }
        }
        Err(_) => {
            return Err("caps.wall_clock_secs must be a positive integer".to_string());
        }
    };

    // Parse idle_secs (optional)
    let idle_secs = if settings.idle_secs.is_empty() {
        None
    } else {
        match settings.idle_secs.parse::<u64>() {
            Ok(val) => {
                if val >= 1 {
                    Some(val)
                } else {
                    return Err("caps.idle_secs must be at least 1".to_string());
                }
            }
            Err(_) => {
                return Err("caps.idle_secs must be a positive integer".to_string());
            }
        }
    };

    // Parse concurrency
    let concurrency = match settings.concurrency.parse::<usize>() {
        Ok(val) => {
            if val >= 1 {
                val
            } else {
                return Err("concurrency must be at least 1".to_string());
            }
        }
        Err(_) => {
            return Err("concurrency must be a positive integer".to_string());
        }
    };

    Ok(SettingsValidation {
        gate_iterations,
        reviewer_iterations,
        wall_clock_secs,
        idle_secs,
        concurrency,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_settings(
        gate_iterations: &str,
        reviewer_iterations: &str,
        wall_clock_secs: &str,
        idle_secs: &str,
        concurrency: &str,
    ) -> Settings {
        use crate::app::SettingsField;
        Settings {
            gate_iterations: gate_iterations.to_string(),
            reviewer_iterations: reviewer_iterations.to_string(),
            wall_clock_secs: wall_clock_secs.to_string(),
            idle_secs: idle_secs.to_string(),
            concurrency: concurrency.to_string(),
            focused: SettingsField::GateIterations,
            error: None,
        }
    }

    #[test]
    fn test_validate_gate_iterations_must_be_at_least_one() {
        let settings = make_settings("0", "1", "30", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "caps.gate_iterations must be at least 1"
        );
    }

    #[test]
    fn test_validate_gate_iterations_must_be_positive_integer() {
        let settings = make_settings("abc", "1", "30", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "caps.gate_iterations must be a positive integer"
        );
    }

    #[test]
    fn test_validate_gate_iterations_valid() {
        let settings = make_settings("5", "1", "30", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_ok());
        let valid = result.unwrap();
        assert_eq!(valid.gate_iterations, 5);
    }

    #[test]
    fn test_validate_reviewer_iterations_must_be_at_least_one() {
        let settings = make_settings("3", "0", "30", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "caps.reviewer_iterations must be at least 1"
        );
    }

    #[test]
    fn test_validate_reviewer_iterations_must_be_positive_integer() {
        let settings = make_settings("3", "xyz", "30", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "caps.reviewer_iterations must be a positive integer"
        );
    }

    #[test]
    fn test_validate_reviewer_iterations_valid() {
        let settings = make_settings("3", "2", "30", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_ok());
        let valid = result.unwrap();
        assert_eq!(valid.reviewer_iterations, 2);
    }

    #[test]
    fn test_validate_wall_clock_secs_must_be_at_least_one() {
        let settings = make_settings("3", "1", "0", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "caps.wall_clock_secs must be at least 1"
        );
    }

    #[test]
    fn test_validate_wall_clock_secs_must_be_positive_integer() {
        let settings = make_settings("3", "1", "invalid", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "caps.wall_clock_secs must be a positive integer"
        );
    }

    #[test]
    fn test_validate_wall_clock_secs_valid() {
        let settings = make_settings("3", "1", "30", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_ok());
        let valid = result.unwrap();
        assert_eq!(valid.wall_clock_secs, 30);
    }

    #[test]
    fn test_validate_idle_secs_empty_is_none() {
        let settings = make_settings("3", "1", "30", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_ok());
        let valid = result.unwrap();
        assert_eq!(valid.idle_secs, None);
    }

    #[test]
    fn test_validate_idle_secs_must_be_at_least_one() {
        let settings = make_settings("3", "1", "30", "0", "4");
        let result = validate_settings(&settings);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "caps.idle_secs must be at least 1");
    }

    #[test]
    fn test_validate_idle_secs_must_be_positive_integer() {
        let settings = make_settings("3", "1", "30", "nope", "4");
        let result = validate_settings(&settings);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "caps.idle_secs must be a positive integer"
        );
    }

    #[test]
    fn test_validate_idle_secs_valid() {
        let settings = make_settings("3", "1", "30", "60", "4");
        let result = validate_settings(&settings);
        assert!(result.is_ok());
        let valid = result.unwrap();
        assert_eq!(valid.idle_secs, Some(60));
    }

    #[test]
    fn test_validate_concurrency_must_be_at_least_one() {
        let settings = make_settings("3", "1", "30", "", "0");
        let result = validate_settings(&settings);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "concurrency must be at least 1");
    }

    #[test]
    fn test_validate_concurrency_must_be_positive_integer() {
        let settings = make_settings("3", "1", "30", "", "oops");
        let result = validate_settings(&settings);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "concurrency must be a positive integer"
        );
    }

    #[test]
    fn test_validate_concurrency_valid() {
        let settings = make_settings("3", "1", "30", "", "4");
        let result = validate_settings(&settings);
        assert!(result.is_ok());
        let valid = result.unwrap();
        assert_eq!(valid.concurrency, 4);
    }

    #[test]
    fn test_validate_all_fields_valid() {
        let settings = make_settings("10", "3", "120", "45", "8");
        let result = validate_settings(&settings);
        assert!(result.is_ok());
        let valid = result.unwrap();
        assert_eq!(valid.gate_iterations, 10);
        assert_eq!(valid.reviewer_iterations, 3);
        assert_eq!(valid.wall_clock_secs, 120);
        assert_eq!(valid.idle_secs, Some(45));
        assert_eq!(valid.concurrency, 8);
    }
}
