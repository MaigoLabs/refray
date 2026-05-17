use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

fn main() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let build_time = OffsetDateTime::from_unix_timestamp(build_timestamp())
        .expect("build timestamp must fit in OffsetDateTime")
        .format(&Rfc3339)
        .expect("build timestamp must format as RFC3339");

    println!("cargo:rustc-env=REFRAY_BUILD_TIME={build_time}");
}

fn build_timestamp() -> i64 {
    match env::var("SOURCE_DATE_EPOCH") {
        Ok(value) => value
            .parse::<u64>()
            .expect("SOURCE_DATE_EPOCH must be a non-negative Unix timestamp")
            .try_into()
            .expect("SOURCE_DATE_EPOCH must fit in i64"),
        Err(env::VarError::NotPresent) => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time must be after the Unix epoch")
            .as_secs()
            .try_into()
            .expect("build timestamp must fit in i64"),
        Err(env::VarError::NotUnicode(_)) => panic!("SOURCE_DATE_EPOCH must be valid Unicode"),
    }
}
