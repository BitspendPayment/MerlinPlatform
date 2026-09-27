//! Where this platform pays out: a country, its currency, and the rails that reach a person there.
//!
//! One table, served to the app as `GET /corridors`, which renders its forms from it — so another
//! country is another row here, not another release of the app. A field is named as Grid names it
//! in `accountInfo`, a payout's `fields` use the same names, and the sealed policy pins every one
//! of them against Grid's record of the account (see `payout`).
//!
//! Checked here before anything reaches Grid or a sealed policy, and again by Grid, which alone
//! knows the bank names it takes (`GET /corridors/{country}/banks`).

use std::collections::BTreeMap;

use serde::Serialize;

/// A country this platform pays out in.
#[derive(Debug, Serialize)]
pub struct Corridor {
    /// ISO 3166-1 alpha-2, as Grid's discoveries take it.
    pub country: &'static str,
    pub name: &'static str,
    /// ISO 4217. Amounts are in its minor units: kobo, cents, pesewas.
    pub currency: &'static str,
    /// How many of its minor units make one: 2 is a hundred.
    pub decimals: u32,
    pub rails: &'static [Rail],
}

/// One way to reach a person there: a bank account, a mobile-money wallet.
#[derive(Debug, Serialize)]
pub struct Rail {
    /// `bank` or `mobile_money`: what a payout names.
    pub rail: &'static str,
    pub label: &'static str,
    /// Grid's `accountInfo.accountType`. Grid's business, not the app's.
    #[serde(skip)]
    pub account_type: &'static str,
    /// What one payout on this rail may be, in minor units.
    pub min_minor: i64,
    pub max_minor: i64,
    /// The payee's account. The payee's name is not among them: every rail needs one.
    pub fields: &'static [Field],
}

/// One thing the customer types or picks.
#[derive(Debug, Serialize)]
pub struct Field {
    /// Grid's name for it in `accountInfo`, and its key in a payout's `fields`.
    pub key: &'static str,
    pub label: &'static str,
    /// `text` or `select`.
    pub kind: &'static str,
    /// Text: what it starts with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<&'static str>,
    /// Text: how many digits follow the prefix. Nothing but digits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digits: Option<Digits>,
    /// Select: one of these, spelled exactly so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<&'static [&'static str]>,
    /// Select: one of the names `GET /corridors/{country}/banks` lists, spelled exactly so.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub from_bank_list: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Digits {
    pub min: usize,
    pub max: usize,
}

const fn digits(key: &'static str, label: &'static str, min: usize, max: usize) -> Field {
    Field {
        key,
        label,
        kind: "text",
        prefix: None,
        digits: Some(Digits { min, max }),
        options: None,
        from_bank_list: false,
    }
}

const fn phone(key: &'static str, label: &'static str, prefix: &'static str, n: usize) -> Field {
    Field {
        prefix: Some(prefix),
        ..digits(key, label, n, n)
    }
}

const fn one_of(key: &'static str, label: &'static str, options: &'static [&'static str]) -> Field {
    Field {
        key,
        label,
        kind: "select",
        prefix: None,
        digits: None,
        options: Some(options),
        from_bank_list: false,
    }
}

const fn bank_list(key: &'static str, label: &'static str) -> Field {
    Field {
        from_bank_list: true,
        options: None,
        ..one_of(key, label, &[])
    }
}

/// Ghana's mobile-money networks, which Grid takes as the `bankName` — only as spelled in its
/// discoveries.
///
/// UNCERTAIN: Grid's docs show "MTN Mobile Money"; the other two are the networks' own names. Check
/// all three against `GET /discoveries?country=GH&currency=GHS` in the sandbox.
const GH_MOBILE_MONEY: &[&str] = &["MTN Mobile Money", "Telecel Cash", "AirtelTigo Money"];

/// Every corridor. Field rules are Grid's own (its OpenAPI `*AccountInfoBase` schemas), narrowed
/// where noted.
///
/// ponytail: pilot limits, about $1 to $1,000 at 2026 rates; Grid's per-corridor limits once known.
pub const CORRIDORS: &[Corridor] = &[
    Corridor {
        country: "NG",
        name: "Nigeria",
        currency: "NGN",
        decimals: 2,
        rails: &[Rail {
            rail: "bank",
            label: "Bank account",
            account_type: "NGN_ACCOUNT",
            min_minor: 150_000,     // ₦1,500
            max_minor: 150_000_000, // ₦1,500,000
            fields: &[
                digits("accountNumber", "Account number", 10, 10),
                bank_list("bankName", "Bank"),
            ],
        }],
    },
    Corridor {
        country: "KE",
        name: "Kenya",
        currency: "KES",
        decimals: 2,
        rails: &[Rail {
            rail: "mobile_money",
            label: "M-PESA",
            account_type: "KES_ACCOUNT",
            min_minor: 13_000,     // KSh 130
            max_minor: 13_000_000, // KSh 130,000
            fields: &[
                phone("phoneNumber", "M-PESA number", "+254", 9),
                one_of("provider", "Provider", &["M-PESA"]),
            ],
        }],
    },
    Corridor {
        country: "GH",
        name: "Ghana",
        currency: "GHS",
        decimals: 2,
        rails: &[
            Rail {
                rail: "bank",
                label: "Bank account",
                account_type: "GHS_ACCOUNT",
                min_minor: 1_500,     // GH₵15
                max_minor: 1_500_000, // GH₵15,000
                // Grid: 1 to 34 characters, any. Narrowed to digits, as Ghana's are.
                fields: &[
                    digits("accountNumber", "Account number", 1, 34),
                    bank_list("bankName", "Bank"),
                ],
            },
            Rail {
                rail: "mobile_money",
                label: "Mobile money",
                account_type: "GHS_ACCOUNT",
                min_minor: 1_500,     // GH₵15
                max_minor: 1_500_000, // GH₵15,000
                // Grid: any `+` and 6 to 14 digits. Narrowed to Ghana's.
                fields: &[
                    phone("phoneNumber", "Mobile money number", "+233", 9),
                    one_of("bankName", "Network", GH_MOBILE_MONEY),
                ],
            },
        ],
    },
    Corridor {
        country: "ZA",
        name: "South Africa",
        currency: "ZAR",
        decimals: 2,
        rails: &[Rail {
            rail: "bank",
            label: "Bank account",
            account_type: "ZAR_ACCOUNT",
            min_minor: 1_800,     // R18
            max_minor: 1_800_000, // R18,000
            fields: &[
                digits("accountNumber", "Account number", 9, 13),
                bank_list("bankName", "Bank"),
            ],
        }],
    },
];

/// The country's corridor, if this platform pays out there.
pub fn country(code: &str) -> Option<&'static Corridor> {
    CORRIDORS.iter().find(|c| c.country == code)
}

/// Every rail of every corridor.
pub fn rails() -> impl Iterator<Item = (&'static Corridor, &'static Rail)> {
    CORRIDORS
        .iter()
        .flat_map(|c| c.rails.iter().map(move |r| (c, r)))
}

/// What a payout names, checked before it goes anywhere: a corridor and rail this platform
/// serves, exactly that rail's fields and each well-formed, a payee's name, and an amount the rail
/// takes.
pub fn validate(
    country: &str,
    rail: &str,
    fields: &BTreeMap<String, String>,
    full_name: &str,
    amount_minor: i64,
) -> Result<(&'static Corridor, &'static Rail), String> {
    let corridor = self::country(country)
        .ok_or_else(|| format!("this platform does not pay out in {country:?}"))?;
    let found = corridor.rails.iter().find(|r| r.rail == rail).ok_or_else(|| {
        let rails: Vec<&str> = corridor.rails.iter().map(|r| r.rail).collect();
        format!("{} is paid out by {}, not {rail:?}", corridor.name, rails.join(" or "))
    })?;
    found.check_fields(fields)?;
    // Grid's limit on a beneficiary's `fullName`.
    if full_name.trim().is_empty() || full_name.chars().count() > 250 {
        return Err("a payee's name is 1 to 250 characters".into());
    }
    if !(found.min_minor..=found.max_minor).contains(&amount_minor) {
        return Err(format!(
            "a payout in {} is {} to {} minor units on this rail",
            corridor.currency, found.min_minor, found.max_minor
        ));
    }
    Ok((corridor, found))
}

impl Rail {
    /// Exactly this rail's fields, each well-formed.
    pub fn check_fields(&self, fields: &BTreeMap<String, String>) -> Result<(), String> {
        let named = |key: &String| self.fields.iter().any(|f| f.key == key.as_str());
        if let Some(other) = fields.keys().find(|k| !named(k)) {
            return Err(format!("{other:?} is not a field of this rail"));
        }
        for field in self.fields {
            let value = fields
                .get(field.key)
                .ok_or_else(|| format!("{} ({}) is missing", field.label, field.key))?;
            field.check(value)?;
        }
        Ok(())
    }
}

impl Field {
    fn check(&self, value: &str) -> Result<(), String> {
        let (fine, rule) = match (self.digits, self.options) {
            (Some(Digits { min, max }), _) => {
                let prefix = self.prefix.unwrap_or("");
                let fine = value.strip_prefix(prefix).is_some_and(|rest| {
                    (min..=max).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_digit())
                });
                let n = if min == max { min.to_string() } else { format!("{min} to {max}") };
                let rule = if prefix.is_empty() {
                    format!("{n} digits")
                } else {
                    format!("{prefix} then {n} digits")
                };
                (fine, rule)
            }
            (_, Some(options)) => {
                (options.contains(&value), format!("one of {}", options.join(", ")))
            }
            // Grid holds the list, and refuses a name that is not on it.
            _ => (
                !value.trim().is_empty() && value.len() <= 255,
                "a name from the bank list".into(),
            ),
        };
        if fine {
            Ok(())
        } else {
            Err(format!("{} ({}) is {rule}", self.label, self.key))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Each rail takes what Grid's schema takes, and refuses a near miss with the rule it broke.
    #[test]
    fn each_rail_takes_its_own_fields_well_formed() {
        let ng = [("accountNumber", "0123456789"), ("bankName", "OPay")];
        let ghm_network = ("bankName", "MTN Mobile Money");
        let gh_mobile = [("phoneNumber", "+233241234567"), ghm_network];
        // A country, a rail, its fields, and what comes of them.
        type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], Result<(), &'a str>);
        let cases: &[Case] = &[
            ("NG", "bank", &ng, Ok(())),
            ("NG", "bank", &[("accountNumber", "012345678"), ("bankName", "OPay")],
                Err("Account number (accountNumber) is 10 digits")),
            ("NG", "bank", &[("accountNumber", "01234567a9"), ("bankName", "OPay")],
                Err("Account number (accountNumber) is 10 digits")),
            ("NG", "bank", &[("accountNumber", "0123456789")], Err("Bank (bankName) is missing")),
            ("NG", "bank", &[("accountNumber", "0123456789"), ("bankName", " ")],
                Err("Bank (bankName) is a name from the bank list")),
            ("KE", "mobile_money", &[("phoneNumber", "+254712345678"), ("provider", "M-PESA")],
                Ok(())),
            ("KE", "mobile_money", &[("phoneNumber", "0712345678"), ("provider", "M-PESA")],
                Err("M-PESA number (phoneNumber) is +254 then 9 digits")),
            ("KE", "mobile_money", &[("phoneNumber", "+25471234567"), ("provider", "M-PESA")],
                Err("M-PESA number (phoneNumber) is +254 then 9 digits")),
            ("KE", "mobile_money", &[("phoneNumber", "+254712345678"), ("provider", "Airtel")],
                Err("Provider (provider) is one of M-PESA")),
            ("GH", "bank", &[("accountNumber", "1234567890123"), ("bankName", "Gcb Bank Ltd")],
                Ok(())),
            ("GH", "bank", &[("accountNumber", &"1".repeat(35)), ("bankName", "Gcb Bank Ltd")],
                Err("Account number (accountNumber) is 1 to 34 digits")),
            ("GH", "mobile_money", &gh_mobile, Ok(())),
            ("GH", "mobile_money", &[("phoneNumber", "+234241234567"), ghm_network],
                Err("Mobile money number (phoneNumber) is +233 then 9 digits")),
            ("GH", "mobile_money", &[("phoneNumber", "+233241234567"), ("bankName", "OPay")],
                Err("Network (bankName) is one of MTN Mobile Money, Telecel Cash, \
                     AirtelTigo Money")),
            // Ghana's two rails share Grid's account type, and not their fields.
            ("GH", "bank", &gh_mobile, Err("\"phoneNumber\" is not a field of this rail")),
            ("ZA", "bank", &[("accountNumber", "123456789"), ("bankName", "Absa Bank")], Ok(())),
            ("ZA", "bank", &[("accountNumber", "12345678"), ("bankName", "Absa Bank")],
                Err("Account number (accountNumber) is 9 to 13 digits")),
            ("ZA", "bank", &[("accountNumber", "12345678901234"), ("bankName", "Absa Bank")],
                Err("Account number (accountNumber) is 9 to 13 digits")),
            ("US", "bank", &ng, Err("this platform does not pay out in \"US\"")),
            ("NG", "mobile_money", &ng, Err("Nigeria is paid out by bank, not \"mobile_money\"")),
        ];
        for (country, rail, pairs, want) in cases {
            let amount = self::country(country).map_or(0, |c| c.rails[0].min_minor);
            let got = validate(country, rail, &fields(pairs), "Ada Obi", amount).map(|_| ());
            assert_eq!(got, want.map_err(str::to_string), "{country} {rail} {pairs:?}");
        }
    }

    #[test]
    fn a_payout_names_its_payee_and_an_amount_the_rail_takes() {
        let ng = fields(&[("accountNumber", "0123456789"), ("bankName", "OPay")]);
        let rail = &CORRIDORS[0].rails[0];
        assert!(validate("NG", "bank", &ng, "Ada Obi", rail.min_minor).is_ok());
        assert!(validate("NG", "bank", &ng, "Ada Obi", rail.max_minor).is_ok());
        assert!(validate("NG", "bank", &ng, "Ada Obi", rail.min_minor - 1).is_err());
        assert!(validate("NG", "bank", &ng, "Ada Obi", rail.max_minor + 1).is_err());
        assert!(validate("NG", "bank", &ng, "  ", rail.min_minor).is_err());
        assert!(validate("NG", "bank", &ng, &"a".repeat(251), rail.min_minor).is_err());
    }

    /// What the app renders its forms from: no Grid internals, and each field says how to check it.
    #[test]
    fn the_table_is_what_the_app_renders_forms_from() {
        let json = serde_json::to_value(CORRIDORS).unwrap();
        assert_eq!(
            json[0]["rails"][0],
            serde_json::json!({
                "rail": "bank",
                "label": "Bank account",
                "min_minor": 150_000,
                "max_minor": 150_000_000,
                "fields": [
                    { "key": "accountNumber", "label": "Account number", "kind": "text",
                      "digits": { "min": 10, "max": 10 } },
                    { "key": "bankName", "label": "Bank", "kind": "select",
                      "from_bank_list": true },
                ],
            })
        );
        assert_eq!(
            json[1]["rails"][0]["fields"],
            serde_json::json!([
                { "key": "phoneNumber", "label": "M-PESA number", "kind": "text", "prefix": "+254",
                  "digits": { "min": 9, "max": 9 } },
                { "key": "provider", "label": "Provider", "kind": "select", "options": ["M-PESA"] },
            ])
        );
    }
}
