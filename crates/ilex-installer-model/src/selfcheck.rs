//! The model's share of `ilex-installerd --self-check`: its rules exercised on
//! fixtures. (The repository has no unit tests; anything worth testing in a
//! shipped binary is observable from inside the VM.)

use crate::answers::Answers;
use crate::catalogue::Table;
use crate::detect::{Identity, ProfileChoice, select};
use crate::edition::{Edition, EditionRecord};
use crate::finish::{FinishAction, check_log_device};
use crate::plan::{ErrorCode, MIN_DISK_BYTES, PlanRequest, check_request};
use crate::probe::{Disk, EditionInfo, Firmware, HelperInfo, Memory, Partition, Probe};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

/// A change to a JSON document, for a variant of a fixture or a request.
type Edit = Box<dyn Fn(&mut Value)>;

pub const TWO_SHELLS: &str = include_str!("../testdata/two-shells.json");

fn mutate(edit: impl FnOnce(&mut Value)) -> Result<Vec<u8>> {
    let mut value: Value = serde_json::from_str(TWO_SHELLS)?;
    edit(&mut value);
    Ok(serde_json::to_vec(&value)?)
}

fn identity(vendor: &str, product: &str, board_vendor: &str, board: &str) -> Identity {
    Identity {
        sys_vendor: vendor.into(),
        product_name: product.into(),
        board_vendor: board_vendor.into(),
        board_name: board.into(),
    }
}

const MSI: &str = "Micro-Star International Co., Ltd.";

fn disk(id: &str, size: u64, live: bool) -> Disk {
    Disk {
        id: id.into(),
        path: format!("/dev/{id}"),
        model: "Fixture".into(),
        serial: id.into(),
        size_bytes: size,
        sector_size: 512,
        bus: "virtio".into(),
        removable: false,
        live_medium: live,
        in_use: vec![],
        notes: vec![],
        partition_table: None,
        partitions: vec![],
        free_extents: vec![],
        other_systems: vec![],
        esps: vec![],
    }
}

fn probe_of(table: &Table, edition: &Edition) -> Result<Probe> {
    let catalogue = table.for_edition(edition)?;
    let detection = table.detect_for_edition(edition, Identity::default())?;
    Ok(Probe {
        schema: 1,
        helper: HelperInfo {
            version: "self-check".into(),
            systemd: None,
        },
        edition: EditionInfo {
            id: edition.id().into(),
            name: edition.fallback_name(table),
        },
        firmware: Firmware {
            uefi: true,
            secure_boot: Some(false),
            setup_mode: Some(false),
        },
        enrollment: json!({"verdict": "unavailable", "reason": "secure-boot-off"}),
        detection,
        selection: None,
        selection_error: None,
        catalogue,
        disks: vec![
            disk("blank", MIN_DISK_BYTES, false),
            disk("live", 64 << 30, true),
            disk("tiny", MIN_DISK_BYTES - 1, false),
            Disk {
                in_use: vec!["/dev/busy1 is mounted at /mnt".into()],
                ..disk("busy", 64 << 30, false)
            },
        ],
        memory: Memory {
            total_bytes: 8 << 30,
            available_bytes: 6 << 30,
        },
    })
}

fn request(edit: impl FnOnce(&mut Value)) -> Result<PlanRequest> {
    let mut value = json!({
        "composition": "generic.gnome", "disk": "blank", "mode": "whole-disk",
        "administrator": "ilexmaint"
    });
    edit(&mut value);
    Ok(serde_json::from_value(value)?)
}

pub fn run() -> Result<String> {
    // The table: the shipped rules, then each way a table can be wrong.
    let table = Table::parse(TWO_SHELLS.as_bytes()).context("fixture table")?;
    let broken: [(&str, Edit); 7] = [
        (
            "repository",
            Box::new(|v| v["repository"] = json!("example.com/ilex")),
        ),
        (
            "tag",
            Box::new(|v| v["compositions"][0]["tag"] = json!("generic.other")),
        ),
        (
            "vm",
            Box::new(|v| {
                v["compositions"][0] =
                    json!({"tag": "vm.gnome", "machine": "vm", "shells": ["gnome"]});
            }),
        ),
        (
            "default",
            Box::new(|v| v["default"] = json!("ms-7c35.gnome")),
        ),
        (
            "dmi pair",
            Box::new(|v| v["machines"]["ms-7c35"]["match"][0] = json!({"sys_vendor": MSI})),
        ),
        (
            "dmi space",
            Box::new(|v| v["machines"]["ms-7c35"]["match"][0]["product_name"] = json!(" MS-7C35")),
        ),
        (
            "profile",
            Box::new(|v| v["profiles"] = json!({"x": {"schema": 2, "display-name": "x"}})),
        ),
    ];
    for (name, edit) in broken {
        ensure!(
            Table::parse(&mutate(edit)?).is_err(),
            "a table with a bad {name} was accepted"
        );
    }

    // Editions: one shell each, no desktop choice.
    let gnome = Edition::new("gnome")?;
    ensure!(
        Edition::of_default(&table)? == gnome,
        "the default's edition is not gnome"
    );
    let view = table.for_edition(&gnome)?;
    // Only the default's own shell list: a composition that merely starts
    // with gnome (generic.gnome.ilex) belongs to another edition.
    ensure!(
        view.compositions
            .iter()
            .map(|c| c.tag.as_str())
            .collect::<Vec<_>>()
            == ["generic.gnome", "ms-7c35.gnome"]
            && view.default == "generic.gnome"
            && !view.shells.contains_key("ilex"),
        "the gnome edition offers {}",
        view.offered()
    );
    ensure!(
        table
            .for_edition(&gnome)?
            .composition("generic.gnome.ilex")
            .is_none(),
        "the gnome edition accepts generic.gnome.ilex"
    );
    ensure!(
        view.machine_offers()?
            .iter()
            .map(|c| c.tag.as_str())
            .collect::<Vec<_>>()
            == ["generic.gnome", "ms-7c35.gnome"],
        "the machine choice is not one entry per machine"
    );
    ensure!(
        table.for_edition(&Edition::new("ilex")?).is_err(),
        "an edition without a generic composition was accepted"
    );
    let record =
        EditionRecord::parse(br#"{"schema":1,"edition":"gnome","display-name":"ilex Glabra"}"#)?;
    ensure!(
        record.edition() == gnome && record.display_name == "ilex Glabra",
        "edition record"
    );
    for bad in [
        r#"{"schema":2,"edition":"gnome","display-name":"x"}"#,
        r#"{"schema":1,"edition":"GNOME","display-name":"x"}"#,
        r#"{"schema":1,"edition":"gnome","display-name":""}"#,
        r#"{"schema":1,"edition":"gnome","display-name":"x","extra":1}"#,
    ] {
        ensure!(
            EditionRecord::parse(bad.as_bytes()).is_err(),
            "edition record accepted: {bad}"
        );
    }

    // Detection: exact and case-sensitive, conflicts named.
    let board_rule = |v: &mut Value| {
        v["machines"]["ms-7c35"]["match"]
            .as_array_mut()
            .unwrap()
            .push(json!({"board_vendor": MSI, "board_name": "MEG X570 UNIFY (MS-7C35)"}));
    };
    let full_with_board = Table::parse(&mutate(board_rule)?)?;
    let with_board = full_with_board.for_edition(&gnome)?;
    let system = full_with_board.detect_for_edition(&gnome, identity(MSI, "MS-7C35", "", ""))?;
    ensure!(
        system.tag == "ms-7c35.gnome" && system.reason == "detected",
        "system match: {system:?}"
    );
    let board = full_with_board
        .detect_for_edition(&gnome, identity("", "", MSI, "MEG X570 UNIFY (MS-7C35)"))?;
    ensure!(
        board.tag == "ms-7c35.gnome" && board.matches == ["ms-7c35"],
        "board-only match: {board:?}"
    );
    let lower = full_with_board.detect_for_edition(
        &gnome,
        identity("micro-star international co., ltd.", "MS-7C35", "", ""),
    )?;
    ensure!(
        lower.tag == "generic.gnome" && lower.matches.is_empty(),
        "matching is not case-sensitive"
    );
    let half = full_with_board.detect_for_edition(&gnome, identity(MSI, "", "", ""))?;
    ensure!(half.matches.is_empty(), "a vendor alone matched");
    // A conflict counts every machine of the catalogue: here the second
    // machine only has a composition in another edition, and the two still
    // fall back to the default with both named.
    let conflict_table = Table::parse(&mutate(|v| {
        v["machines"]["other"] = json!({"description": "Another", "match": [{"sys_vendor": MSI, "product_name": "MS-7C35"}]});
        v["compositions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"tag": "other.ilex", "machine": "other", "shells": ["ilex"]}));
    })?)?;
    let conflict = conflict_table.detect_for_edition(&gnome, identity(MSI, "MS-7C35", "", ""))?;
    ensure!(
        conflict.tag == "generic.gnome"
            && conflict.reason == "matches more than one machine configuration: ms-7c35, other"
            && conflict.matches == ["ms-7c35", "other"],
        "conflict: {conflict:?}"
    );
    // The Anaconda chooser's own rule on the whole table, kept for parity.
    let chooser = conflict_table.detect(identity(MSI, "MS-7C35", "", ""))?;
    ensure!(
        chooser.tag == "generic.gnome" && chooser.reason == conflict.reason,
        "chooser conflict: {chooser:?}"
    );
    // A recognised machine with a composition only in another edition (or
    // another shell list) is reported as having none here.
    let no_composition = Table::parse(&mutate(|v| {
        v["compositions"] = json!([
            {"tag": "generic.gnome", "machine": "generic", "shells": ["gnome"]},
            {"tag": "ms-7c35.gnome.ilex", "machine": "ms-7c35", "shells": ["gnome", "ilex"]}
        ]);
    })?)?
    .detect_for_edition(&gnome, identity(MSI, "MS-7C35", "", ""))?;
    ensure!(
        no_composition.tag == "generic.gnome"
            && no_composition.matches == ["ms-7c35"]
            && no_composition.reason
                == "detected machine ms-7c35 has no composition in the gnome edition",
        "no composition: {no_composition:?}"
    );
    ensure!(
        identity("", "", MSI, "x").summary() == format!("{MSI}, x"),
        "identity summary"
    );

    // Selection: boot argument, then the choice file, then detection.
    let selected = select(&with_board, &system, "quiet", None)?;
    ensure!(
        selected.serial_line()
            == "ILEX-COMPOSITION ms-7c35.gnome (detected) installs ghcr.io/reidond/ilex:ms-7c35.gnome",
        "{}",
        selected.serial_line()
    );
    let boot = select(
        &with_board,
        &system,
        "a ilex.composition=generic.gnome b ilex.composition=ms-7c35.gnome",
        Some("generic.gnome"),
    )?;
    ensure!(
        boot.tag == "ms-7c35.gnome" && boot.reason == "chosen at boot",
        "boot override: {boot:?}"
    );
    let plain = select(&with_board, &system, "", Some("generic.gnome\n"))?;
    ensure!(
        plain.tag == "generic.gnome" && plain.reason == "chosen in the installer",
        "choice file: {plain:?}"
    );
    let record = select(
        &with_board,
        &system,
        "",
        Some(r#"{"schema":1,"composition":"ms-7c35.gnome","profile":null}"#),
    )?;
    ensure!(
        record.profile == Some(ProfileChoice::Custom),
        "choice record: {record:?}"
    );
    ensure!(
        select(
            &with_board,
            &system,
            "",
            Some(r#"{"schema":2,"composition":"generic.gnome"}"#)
        )
        .is_err(),
        "newer record accepted"
    );
    ensure!(
        select(
            &with_board,
            &system,
            "",
            Some(r#"{"schema":1,"composition":"generic.gnome","profile":"nobody"}"#)
        )
        .is_err(),
        "unknown profile accepted"
    );
    let unknown = select(&with_board, &system, "ilex.composition=ms-7c35.kde", None)
        .unwrap_err()
        .to_string();
    ensure!(
        unknown
            == format!(
                "unknown composition 'ms-7c35.kde'; offered: {}",
                with_board.offered()
            ),
        "{unknown}"
    );
    let mut testing = with_board.clone();
    testing.testing_tag = Some("latest".into());
    ensure!(
        select(&testing, &system, "", None)?.target == "ghcr.io/reidond/ilex:latest",
        "testing tag"
    );

    // Requests: syntax, then the probe and the catalogue.
    let probe = probe_of(&table, &gnome)?;
    let ok = check_request(&request(|_| {})?, &probe).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ensure!(
        ok.request.tpm == Some(crate::plan::TpmMode::Auto) && ok.request.encrypt,
        "defaults"
    );
    let refusals: [(&str, ErrorCode, Edit); 25] = [
        (
            "unknown tag",
            ErrorCode::UnknownComposition,
            Box::new(|v| v["composition"] = json!("generic.kde")),
        ),
        (
            "vm",
            ErrorCode::UnknownComposition,
            Box::new(|v| v["composition"] = json!("vm.gnome")),
        ),
        (
            "other edition",
            ErrorCode::UnknownComposition,
            Box::new(|v| v["composition"] = json!("ms-7c35.ilex")),
        ),
        (
            "unknown disk",
            ErrorCode::UnknownDisk,
            Box::new(|v| v["disk"] = json!("nonesuch")),
        ),
        (
            "live medium",
            ErrorCode::LiveMedium,
            Box::new(|v| v["disk"] = json!("live")),
        ),
        (
            "in use",
            ErrorCode::InUse,
            Box::new(|v| v["disk"] = json!("busy")),
        ),
        (
            "too small",
            ErrorCode::DiskTooSmall,
            Box::new(|v| v["disk"] = json!("tiny")),
        ),
        (
            // The fixture's blank disk has no partition table: nothing is
            // beside ilex there.
            "free space on a blank disk",
            ErrorCode::FreeSpaceUnavailable,
            Box::new(|v| v["mode"] = json!("free-space")),
        ),
        (
            "tpm without encryption",
            ErrorCode::TpmNeedsEncryption,
            Box::new(|v| {
                v["encrypt"] = json!(false);
                v["tpm"] = json!("auto");
            }),
        ),
        (
            "reserved system",
            ErrorCode::ReservedName,
            Box::new(|v| v["administrator"] = json!("root")),
        ),
        (
            "reserved profile",
            ErrorCode::InvalidField,
            Box::new(|v| v["administrator"] = json!("Ilexmaint")),
        ),
        (
            "language",
            ErrorCode::InvalidField,
            Box::new(|v| v["language"] = json!("english")),
        ),
        (
            "keyboard",
            ErrorCode::InvalidField,
            Box::new(|v| v["keyboard"] = json!("../us")),
        ),
        (
            "time zone",
            ErrorCode::InvalidField,
            Box::new(|v| v["time_zone"] = json!("../../etc/passwd")),
        ),
        (
            "host name",
            ErrorCode::InvalidField,
            Box::new(|v| v["host_name"] = json!("-bad")),
        ),
        (
            "user name",
            ErrorCode::InvalidField,
            Box::new(|v| v["administrator"] = json!("a b")),
        ),
        (
            "user name of 32 bytes",
            ErrorCode::InvalidField,
            Box::new(|v| v["administrator"] = json!("a".repeat(32))),
        ),
        (
            "reserved service account",
            ErrorCode::ReservedName,
            Box::new(|v| v["administrator"] = json!("sshd")),
        ),
        (
            "reserved marker group",
            ErrorCode::ReservedName,
            Box::new(|v| v["administrator"] = json!("ilex-homed")),
        ),
        (
            "reserved systemd account",
            ErrorCode::ReservedName,
            Box::new(|v| v["administrator"] = json!("systemd-oops")),
        ),
        (
            "language without a codeset",
            ErrorCode::InvalidField,
            Box::new(|v| v["language"] = json!("en_US")),
        ),
        (
            "language in upper case",
            ErrorCode::InvalidField,
            Box::new(|v| v["language"] = json!("EN_us.UTF-8")),
        ),
        (
            "time zone below posix",
            ErrorCode::InvalidField,
            Box::new(|v| v["time_zone"] = json!("posix/Europe/Berlin")),
        ),
        (
            "time zone below right",
            ErrorCode::InvalidField,
            Box::new(|v| v["time_zone"] = json!("right/Europe/Berlin")),
        ),
        (
            "posixrules",
            ErrorCode::InvalidField,
            Box::new(|v| v["time_zone"] = json!("posixrules")),
        ),
    ];
    for (name, code, edit) in refusals {
        let error = check_request(&request(edit)?, &probe).err();
        ensure!(
            error.as_ref().map(|e| e.code) == Some(code),
            "{name}: {error:?}"
        );
    }
    // UEFI only: a computer that booted in BIOS mode is refused before
    // anything else is looked at, however valid the request.
    let mut bios = probe_of(&table, &gnome)?;
    bios.firmware.uefi = false;
    let error = check_request(&request(|_| {})?, &bios).err();
    ensure!(
        error.as_ref().map(|e| e.code) == Some(ErrorCode::UnsupportedFirmware)
            && error.as_ref().is_some_and(|e| e.message.contains("BIOS")),
        "a BIOS boot was not refused: {error:?}"
    );
    let profiled = Table::parse(&mutate(|v| {
        v["profiles"] = json!({"andriishafar": {"schema": 1, "display-name": "Andrii Shafar"}});
    })?)?;
    let error = check_request(
        &request(|v| v["administrator"] = json!("andriishafar"))?,
        &probe_of(&profiled, &gnome)?,
    )
    .err();
    ensure!(
        error.map(|e| e.code) == Some(ErrorCode::ReservedName),
        "a profile's name is not reserved"
    );
    // The daily user: a public profile of the catalogue, or a name of the
    // person's choosing with an optional capacity, on an encrypted disk, and
    // never the maintenance administrator's name.
    let with_profile = probe_of(&profiled, &gnome)?;
    let profile = json!({"kind": "profile", "id": "andriishafar"});
    let custom = |name: &str, size: Option<u64>| {
        let mut daily = json!({"kind": "custom", "name": name});
        if let Some(size) = size {
            daily["disk_size_gib"] = json!(size);
        }
        daily
    };
    for (name, daily) in [
        ("a profile", profile.clone()),
        ("a custom user", custom("alice", None)),
        ("a custom user with a capacity", custom("alice", Some(40))),
        ("the smallest capacity", custom("alice", Some(2))),
        ("the largest capacity", custom("alice", Some(1_048_576))),
    ] {
        let checked = check_request(&request(|v| v["daily"] = daily.clone())?, &with_profile)
            .map_err(|e| anyhow::anyhow!("{name}: {e:?}"))?;
        ensure!(
            serde_json::to_value(&checked.request.daily)? == daily,
            "{name} did not survive the plan: {:?}",
            checked.request.daily
        );
    }
    ensure!(
        serde_json::to_value(
            check_request(&request(|_| {})?, &with_profile)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?
                .request
        )?
        .get("daily")
        .is_none(),
        "a request with no daily user says something of one"
    );
    let daily_refusals: [(&str, ErrorCode, serde_json::Value, bool); 11] = [
        (
            "an unknown profile",
            ErrorCode::InvalidField,
            json!({"kind": "profile", "id": "nobody"}),
            true,
        ),
        (
            "a profile on an unencrypted disk",
            ErrorCode::InvalidField,
            profile.clone(),
            false,
        ),
        (
            "a custom user on an unencrypted disk",
            ErrorCode::InvalidField,
            custom("alice", None),
            false,
        ),
        (
            "a system account",
            ErrorCode::ReservedName,
            custom("root", None),
            true,
        ),
        (
            "the maintenance name",
            ErrorCode::ReservedName,
            custom("ilexmaint", None),
            true,
        ),
        (
            "an upper-case name",
            ErrorCode::InvalidField,
            custom("Alice", None),
            true,
        ),
        (
            "a name that starts with an underscore",
            ErrorCode::InvalidField,
            custom("_alice", None),
            true,
        ),
        (
            "a name that starts with a digit",
            ErrorCode::InvalidField,
            custom("9alice", None),
            true,
        ),
        (
            "a name of 32 bytes",
            ErrorCode::InvalidField,
            custom(&"a".repeat(32), None),
            true,
        ),
        (
            "a capacity of 1 GiB",
            ErrorCode::InvalidField,
            custom("alice", Some(1)),
            true,
        ),
        (
            "a capacity over the limit",
            ErrorCode::InvalidField,
            custom("alice", Some(1_048_577)),
            true,
        ),
    ];
    for (name, code, daily, encrypt) in daily_refusals {
        let error = check_request(
            &request(|v| {
                v["daily"] = daily.clone();
                v["encrypt"] = json!(encrypt);
                v["tpm"] = json!("off");
            })?,
            &with_profile,
        )
        .err();
        ensure!(
            error.as_ref().map(|e| (e.code, e.field.as_deref())) == Some((code, Some("daily"))),
            "{name}: {error:?}"
        );
    }
    ensure!(
        PlanRequest::parse(r#"{"composition":"generic.gnome","disk":"blank","mode":"whole-disk","administrator":"a","daily":{"kind":"root"}}"#).is_err()
            && PlanRequest::parse(r#"{"composition":"generic.gnome","disk":"blank","mode":"whole-disk","administrator":"a","daily":{"kind":"profile","id":"x","password":"y"}}"#).is_err(),
        "an unknown kind or field of the daily user was accepted"
    );
    ensure!(
        PlanRequest::parse(r#"{"composition":"generic.gnome","disk":"blank","mode":"whole-disk","administrator":"a","password":"x"}"#).is_err(),
        "an unknown field was accepted"
    );
    // A refused request never quotes what the caller sent as a value.
    for text in [
        r#"{"composition":"generic.gnome","disk":"blank","mode":"hunter2","administrator":"a"}"#,
        r#"{"composition":"generic.gnome","disk":"blank","mode":"whole-disk","encrypt":"hunter2","administrator":"a"}"#,
        r#"{"composition":"generic.gnome","disk":"blank","mode":"whole-disk","administrator":"a","password":"hunter2"}"#,
        r#"{"composition":"generic.gnome","disk":["hunter2"],"mode":"whole-disk","administrator":"a"}"#,
        r#"{"composition": hunter2}"#,
    ] {
        let error = PlanRequest::parse(text)
            .err()
            .context("a bad request was accepted")?;
        ensure!(
            error.code == ErrorCode::InvalidRequest && !error.message.contains("hunter2"),
            "the refusal quotes a value: {error:?}"
        );
    }
    let missing = PlanRequest::parse(r#"{"composition":"x"}"#)
        .err()
        .context("accepted")?;
    ensure!(
        missing.message.starts_with("missing field `disk`"),
        "missing field: {missing:?}"
    );

    // Answers.
    let text = r#"
schema = 1
[install]
disk = "blank"
mode = "whole-disk"
confirm-erase = "blank"
[encryption]
passphrase = "correct horse"
[administrator]
name = "ilexmaint"
password = "hunter2"
"#;
    let answers = Answers::parse(text)?;
    let from_answers = answers
        .to_request("generic.gnome")
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ensure!(
        from_answers.composition == "generic.gnome" && from_answers.encrypt,
        "answers request"
    );
    ensure!(
        !format!("{answers:?}").contains("hunter2")
            && !format!("{answers:?}").contains("correct horse"),
        "answers print their secrets"
    );
    // `[daily]`: a profile, or a name with an optional capacity, nothing else.
    let daily_of = |extra: &str| -> Result<Option<crate::plan::DailyUser>> {
        Ok(Answers::parse(&format!("{text}{extra}"))?
            .to_request("generic.gnome")
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .daily)
    };
    ensure!(
        from_answers.daily.is_none()
            && daily_of("[daily]\nprofile = \"andriishafar\"\n")?
                == Some(crate::plan::DailyUser::Profile {
                    id: "andriishafar".into()
                })
            && daily_of("[daily]\nname = \"alice\"\ndisk-size = 40\n")?
                == Some(crate::plan::DailyUser::Custom {
                    name: "alice".into(),
                    disk_size_gib: Some(40)
                })
            && daily_of("[daily]\nname = \"alice\"\n")?
                == Some(crate::plan::DailyUser::Custom {
                    name: "alice".into(),
                    disk_size_gib: None
                }),
        "answers [daily]"
    );
    for extra in [
        "[daily]\n",
        "[daily]\ndisk-size = 40\n",
        "[daily]\nprofile = \"andriishafar\"\nname = \"alice\"\n",
        "[daily]\nprofile = \"andriishafar\"\ndisk-size = 40\n",
        "[daily]\nname = \"Alice\"\n",
        "[daily]\nname = \"alice\"\ndisk-size = 1\n",
        "[daily]\nname = \"alice\"\npassword = \"x\"\n",
    ] {
        ensure!(daily_of(extra).is_err(), "answers {extra:?} were accepted");
    }
    // A disk that already holds a system is erased only when the answers say
    // so, explicitly.
    ensure!(
        !answers.install.reinstall
            && Answers::parse(&text.replace("[install]", "[install]\nreinstall = true"))?
                .install
                .reinstall,
        "reinstall is off unless the answers turn it on"
    );
    ensure!(
        Answers::parse(&text.replace("[install]", "[install]\nreinstall = \"yes\"")).is_err(),
        "a reinstall that is not a boolean was accepted"
    );
    // Parse errors say where, never what: the file holds passwords.
    let error = Answers::parse(&text.replace("mode = \"whole-disk\"", "mode = hunter2 wrongly"))
        .err()
        .context("broken answers were accepted")?
        .to_string();
    ensure!(
        error.contains("line 5") && !error.contains("hunter2") && !error.contains("whole-disk"),
        "answers error: {error}"
    );
    let error = Answers::parse(&text.replace("password = \"hunter2\"", "password = 12345"))
        .err()
        .context("a numeric password was accepted")?
        .to_string();
    ensure!(
        !error.contains("12345") && error.contains("withheld"),
        "answers error: {error}"
    );
    for (name, text) in [
        (
            "line break in the password",
            text.replace("password = \"hunter2\"", "password = \"hunter2\\nmore\""),
        ),
        (
            "NUL in the password",
            text.replace("password = \"hunter2\"", "password = \"hunter2\\u0000\""),
        ),
        (
            "NUL in the passphrase",
            text.replace("correct horse", "correct\\u0000horse"),
        ),
        ("erase", text.replace("confirm-erase = \"blank\"\n", "")),
        (
            "passphrase",
            text.replace("passphrase = \"correct horse\"", ""),
        ),
        ("field", text.replace("[install]", "[install]\nextra = 1")),
        ("schema", text.replace("schema = 1", "schema = 2")),
    ] {
        ensure!(
            Answers::parse(&text)
                .and_then(|a| a.to_request("generic.gnome").map_err(Into::into))
                .is_err(),
            "answers with a bad {name} were accepted"
        );
    }

    // What Apply adds: secrets by kind, the new refusals' names on the wire,
    // the stages in order, and a state document an older helper wrote.
    use crate::plan::{SECRET_MAX_BYTES, SecretKind};
    use crate::state::{Outcome, Stage, State};
    ensure!(
        SecretKind::parse("admin-password") == Some(SecretKind::AdminPassword)
            && SecretKind::parse("disk-passphrase") == Some(SecretKind::DiskPassphrase)
            && SecretKind::parse("root-password").is_none()
            && SecretKind::parse("").is_none()
            && [SecretKind::AdminPassword, SecretKind::DiskPassphrase]
                .iter()
                .all(|k| SecretKind::parse(k.name()) == Some(*k))
            && SECRET_MAX_BYTES >= 1024,
        "secret kinds"
    );
    for (code, name) in [
        (ErrorCode::UnknownPlan, "unknown-plan"),
        (ErrorCode::PlanExpired, "plan-expired"),
        (ErrorCode::DiskChanged, "disk-changed"),
        (ErrorCode::MissingSecret, "missing-secret"),
        (ErrorCode::InvalidSecret, "invalid-secret"),
        (ErrorCode::InstallationActive, "installation-active"),
        (ErrorCode::Signature, "signature"),
        (ErrorCode::NotRemovable, "not-removable"),
        (ErrorCode::InstallTarget, "install-target"),
        (ErrorCode::NoFileSystem, "no-file-system"),
    ] {
        ensure!(
            serde_json::to_value(code)? == json!(name),
            "the wire name of {code:?}"
        );
    }
    ensure!(
        Stage::UNENCRYPTED.windows(2).all(|w| w[0] < w[1])
            && Stage::UNENCRYPTED.iter().all(|s| s.name()
                == serde_json::to_value(s)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()),
        "the stages are not in order or their names are not their wire names"
    );
    let old: State = serde_json::from_value(json!({
        "schema": 1, "stage": "idle", "plan": null, "progress": 0.0,
        "message": "", "outcome": null, "updated": 1
    }))?;
    ensure!(
        old == State::idle(1),
        "a state document from before Apply does not read as an idle one"
    );
    ensure!(
        serde_json::to_value(Outcome::Failed {
            stage: Stage::Image,
            reason: "x".into()
        })? == json!({"result": "failed", "stage": "image", "reason": "x"}),
        "the outcome's wire form"
    );
    finish_rules()?;
    recovery_rules()?;
    free_space_rules(&table)?;
    Ok(
        "table rules, editions, detection, selection, requests, answers, secrets, stages, finish, recovery key, free space"
            .into(),
    )
}

/// A partition of a stick: `fs` is what `lsblk` calls its file system.
fn stick_partition(number: u32, fs: &str, size: u64) -> Partition {
    Partition {
        number,
        path: format!("/dev/stick{number}"),
        start_bytes: 1 << 20,
        size_bytes: size,
        type_id: String::new(),
        label: String::new(),
        filesystem: fs.into(),
        filesystem_label: String::new(),
        uuid: String::new(),
    }
}

/// `Finish`: the action's grammar, and which disks a log may be written to.
fn finish_rules() -> Result<()> {
    for (text, action) in [
        ("restart", FinishAction::Restart),
        ("poweroff", FinishAction::PowerOff),
        (
            "save-log:usb-QEMU_HARDDISK_1-0:0",
            FinishAction::SaveLog {
                device: "usb-QEMU_HARDDISK_1-0:0".into(),
            },
        ),
    ] {
        ensure!(
            FinishAction::parse(text).as_ref() == Ok(&action) && action.text() == text,
            "the action {text} does not round-trip"
        );
    }
    for text in [
        "",
        "Restart",
        "reboot",
        "restart ",
        "save-log:",
        "save-log:a/b",
        "save-log:../x",
        "save-log:.hidden",
        "save-log:a\nb",
        "restart:now",
    ] {
        let Err(error) = FinishAction::parse(text) else {
            anyhow::bail!("the action {text:?} is accepted");
        };
        ensure!(
            error.code == ErrorCode::InvalidField
                && error.field.as_deref() == Some("action")
                // It never quotes what it refuses (the prefix is part of its
                // own wording).
                && (text.len() <= "save-log:".len() || !error.message.contains(text)),
            "the refusal of {text:?} is {error:?}"
        );
    }

    let table = Table::parse(TWO_SHELLS.as_bytes()).context("fixture table")?;
    let mut probe = probe_of(&table, &Edition::new("gnome")?)?;
    let stick = |id: &str| Disk {
        removable: true,
        partitions: vec![stick_partition(1, "vfat", 64 << 20)],
        ..disk(id, 8 << 30, false)
    };
    probe.disks = vec![
        stick("usb-stick"),
        Disk {
            removable: true,
            partitions: vec![stick_partition(1, "vfat", 64 << 20)],
            ..disk("usb-live", 8 << 30, true)
        },
        Disk {
            removable: true,
            partitions: vec![stick_partition(1, "vfat", 64 << 20)],
            ..disk("usb-target", 64 << 30, false)
        },
        Disk {
            partitions: vec![stick_partition(1, "vfat", 64 << 20)],
            ..disk("fixed", 64 << 30, false)
        },
        Disk {
            in_use: vec!["/dev/stick1 is mounted at /mnt".into()],
            ..stick("usb-busy")
        },
        Disk {
            partitions: vec![],
            ..stick("usb-blank")
        },
        Disk {
            partitions: vec![stick_partition(1, "crypto_LUKS", 64 << 20)],
            ..stick("usb-luks")
        },
        Disk {
            partitions: vec![stick_partition(1, "vfat", 1 << 20)],
            ..stick("usb-tiny")
        },
        Disk {
            partitions: vec![
                stick_partition(1, "swap", 64 << 20),
                stick_partition(2, "ext4", 64 << 20),
            ],
            ..stick("usb-second")
        },
    ];
    let target = Some("usb-target");
    let ok = check_log_device(&probe, "usb-stick", target)?;
    ensure!(
        ok.0.id == "usb-stick" && ok.1.number == 1,
        "the stick is not the log's place"
    );
    let second = check_log_device(&probe, "usb-second", target)?;
    ensure!(
        second.1.number == 2,
        "the first partition with a file system the helper writes is not chosen"
    );
    for (id, code) in [
        ("nothing", ErrorCode::UnknownDisk),
        ("usb-live", ErrorCode::LiveMedium),
        ("usb-target", ErrorCode::InstallTarget),
        ("fixed", ErrorCode::NotRemovable),
        ("usb-busy", ErrorCode::InUse),
        ("usb-blank", ErrorCode::NoFileSystem),
        ("usb-luks", ErrorCode::NoFileSystem),
        ("usb-tiny", ErrorCode::NoFileSystem),
    ] {
        let Err(error) = check_log_device(&probe, id, target) else {
            anyhow::bail!("{id} is accepted as a place for the log");
        };
        ensure!(
            error.code == code && error.field.as_deref() == Some("device"),
            "{id} is refused with {error:?}, expected {code:?}"
        );
    }
    // The installation's own disk is refused whatever else is true of it, and
    // a fixed disk that is also the target says so, not "not removable".
    let Err(error) = check_log_device(&probe, "fixed", Some("fixed")) else {
        anyhow::bail!("the target is accepted when it is a fixed disk");
    };
    ensure!(
        error.code == ErrorCode::InstallTarget,
        "the target that is a fixed disk is refused as {:?}",
        error.code
    );
    Ok(())
}

/// The recovery key's shape, which groups are asked and what matches.
pub const TEST_KEY: &str =
    "cdefghij-klnrtuvc-dbcdefgh-ijklnrtu-vcbdefgh-ijklnrtu-vcdefghi-jklnrtuv";

fn recovery_rules() -> Result<()> {
    use crate::recovery::{self, KeyProblem};
    ensure!(
        recovery::is_key(TEST_KEY)
            && recovery::groups(TEST_KEY).len() == recovery::GROUPS
            && TEST_KEY.len() == recovery::KEY_CHARS,
        "a recovery key of systemd's shape"
    );
    for not in [
        "",
        "cdefghij-klnrtuvc",
        &TEST_KEY.replace('c', "a"),
        &TEST_KEY.to_ascii_uppercase(),
        &format!("{TEST_KEY}-cdefghij"),
        &TEST_KEY.replace('-', ""),
    ] {
        ensure!(!recovery::is_key(not), "{not:?} is not a recovery key");
    }
    // What a person types for the key: the paper's hyphens, spaces and capitals
    // are forgiven; the wrong length and letters the key never has are not.
    ensure!(
        recovery::normalize(" CDEF-ghij ") == "cdefghij"
            && recovery::key_problem(TEST_KEY).is_none()
            && recovery::key_problem(&TEST_KEY.replace('-', " ").to_uppercase()).is_none()
            && recovery::key_problem(&TEST_KEY.replace('-', "")).is_none()
            && recovery::key_problem("") == Some(KeyProblem::Empty)
            && recovery::key_problem("cdefghij-klnr") == Some(KeyProblem::Short { letters: 12 })
            && recovery::key_problem(&format!("{TEST_KEY}c")) == Some(KeyProblem::Long)
            && recovery::key_problem("cdefghia") == Some(KeyProblem::NotInKey { group: 1 })
            && recovery::key_problem("cdefghij-klnrtuv1")
                == Some(KeyProblem::NotInKey { group: 2 })
            && recovery::key_problem(&TEST_KEY.replacen('v', "w", 6))
                == Some(KeyProblem::NotInKey { group: 2 }),
        "typing the key"
    );
    // The answer's text is the whole key as the paper has it, and every group
    // of it matters.
    ensure!(
        recovery::canonical(&TEST_KEY.to_uppercase().replace('-', " ")).as_deref()
            == Some(TEST_KEY)
            && recovery::canonical("cdefghij").is_none()
            && recovery::matches(TEST_KEY, TEST_KEY)
            && recovery::matches(TEST_KEY, &TEST_KEY.replace('-', "").to_uppercase()),
        "a right answer"
    );
    for group in 0..recovery::GROUPS {
        let mut typed: Vec<char> = TEST_KEY.chars().collect();
        // One letter of this group changed to another letter of the alphabet.
        let at = group * (recovery::GROUP_CHARS + 1) + 3;
        typed[at] = if typed[at] == 'c' { 'b' } else { 'c' };
        let typed: String = typed.into_iter().collect();
        ensure!(
            recovery::key_problem(&typed).is_none() && !recovery::matches(TEST_KEY, &typed),
            "a changed letter in group {} matched",
            group + 1
        );
    }
    ensure!(
        !recovery::matches(TEST_KEY, "cdefghij-klnrtuvc")
            && !recovery::matches(TEST_KEY, "")
            && !recovery::matches("not a key", TEST_KEY),
        "a short answer or a key that is not one matched"
    );
    // Every reason a security chip is not used has a sentence.
    for code in [
        "secure-boot-off",
        "no-tpm",
        "pcr7-unmeasured",
        "tool-mismatch",
        "unlock-stack-missing",
        "not-requested",
        "no-recovery-key",
        "recovery-key-failed",
        "enrollment-failed",
    ] {
        ensure!(
            !recovery::tpm_reason(code).starts_with("reason code"),
            "no sentence for {code}"
        );
    }
    ensure!(
        serde_json::to_value(recovery::KeyStatus::Shown)? == json!("shown")
            && serde_json::to_value(recovery::KeyStatus::Declined)? == json!("declined")
            && serde_json::to_value(recovery::TpmStatus::EnrolledUnverified)?
                == json!("enrolled-unverified")
            && serde_json::to_value(recovery::TpmStatus::PassphraseOnly)?
                == json!("passphrase-only"),
        "the status words are setup.json's"
    );
    Ok(())
}

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn extent(start: u64, size: u64) -> crate::probe::Extent {
    crate::probe::Extent {
        start_bytes: start,
        size_bytes: size,
    }
}

fn part(
    number: u32,
    start: u64,
    size: u64,
    type_id: &str,
    label: &str,
    fs: &str,
    uuid: &str,
) -> Partition {
    Partition {
        number,
        path: format!("/dev/fixture{number}"),
        start_bytes: start,
        size_bytes: size,
        type_id: type_id.into(),
        label: label.into(),
        filesystem: fs.into(),
        filesystem_label: String::new(),
        uuid: uuid.into(),
    }
}

/// A Windows-like disk of 200 GiB: an ESP of 100 MiB with `Microsoft`, the
/// MSR, 20 GiB of NTFS, a 5 GiB gap, the recovery partition and the rest free.
fn windows_disk(id: &str) -> Disk {
    use crate::free_space::{ESP_TYPE, ROOT_X86_64_TYPE};
    let _ = ROOT_X86_64_TYPE;
    let esp = part(
        1,
        MIB,
        100 * MIB,
        ESP_TYPE,
        "EFI system partition",
        "vfat",
        "aaaaaaaa-0000-4000-8000-000000000001",
    );
    let msr = part(
        2,
        101 * MIB,
        16 * MIB,
        "e3c9e316-0b5c-4db8-817d-f92df00215ae",
        "",
        "",
        "aaaaaaaa-0000-4000-8000-000000000002",
    );
    let ntfs = part(
        3,
        117 * MIB,
        20 * GIB,
        "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7",
        "Basic data partition",
        "ntfs",
        "aaaaaaaa-0000-4000-8000-000000000003",
    );
    let recovery_start = 117 * MIB + 25 * GIB;
    let recovery = part(
        4,
        recovery_start,
        500 * MIB,
        "de94bba4-06d1-4d40-a16a-bfd50179d6ac",
        "",
        "ntfs",
        "aaaaaaaa-0000-4000-8000-000000000004",
    );
    let recovery_end = recovery_start + 500 * MIB;
    Disk {
        partition_table: Some("gpt".into()),
        partitions: vec![esp, msr, ntfs, recovery],
        free_extents: vec![
            extent(117 * MIB + 20 * GIB, 5 * GIB),
            extent(recovery_end, 200 * GIB - MIB - recovery_end),
        ],
        other_systems: vec![crate::probe::OtherSystem {
            kind: "efi-loader".into(),
            partition: Some("/dev/fixture1".into()),
            description: "Windows Boot Manager (EFI/Microsoft)".into(),
        }],
        esps: vec![crate::probe::EspInfo {
            partition: "/dev/fixture1".into(),
            readable: true,
            total_bytes: Some(96 * MIB),
            free_bytes: Some(60 * MIB),
            loaders: vec!["Microsoft".into()],
            fedora_directory: false,
        }],
        ..disk(id, 200 * GIB, false)
    }
}

/// Free-space mode: where `systemd-repart` puts the two partitions (the
/// helper's own self-check compares this rule with repart's dry run on sparse
/// disks) and the refusals.
fn free_space_rules(table: &Table) -> Result<()> {
    use crate::free_space::{
        BIOS_BOOT_TYPE, ESP_TYPE, MIN_GAP_BYTES, ROOT_X86_64_TYPE, XBOOTLDR_TYPE, assess, place,
    };
    // Best fit: each partition goes to the smallest area that holds it.
    let two = place(&[extent(10 * GIB, 5 * GIB), extent(20 * GIB, 84 * GIB)])
        .context("a 5 GiB and an 84 GiB gap")?;
    ensure!(
        two.boot == extent(10 * GIB, 5 * GIB) && two.root == extent(20 * GIB, 84 * GIB),
        "/boot goes to the small gap, the root to the big one: {two:?}"
    );
    let one = place(&[extent(0, 80 * GIB), extent(100 * GIB, 118 * GIB)])
        .context("an 80 GiB and a 118 GiB gap")?;
    ensure!(
        one.boot == extent(0, 80 * GIB) && one.root == extent(GIB, 79 * GIB),
        "both go to the smaller gap, the root after /boot: {one:?}"
    );
    let tail = place(&[extent(0, 100 * GIB), extent(150 * GIB, 99 * GIB)])
        .context("a 100 GiB and a 99 GiB gap")?;
    ensure!(
        tail.boot == extent(150 * GIB, 99 * GIB) && tail.root == extent(151 * GIB, 98 * GIB),
        "both go to the smaller gap: {tail:?}"
    );
    ensure!(
        place(&[extent(0, MIN_GAP_BYTES)]).is_some()
            && place(&[extent(0, MIN_GAP_BYTES - MIB)]).is_none()
            && place(&[extent(0, 40 * GIB - MIB), extent(80 * GIB, 20 * GIB)]).is_some()
            && place(&[extent(0, 2 * GIB), extent(10 * GIB, 29 * GIB)]).is_none()
            && place(&[]).is_none(),
        "the least free space"
    );

    // A disk that can be installed beside, and each way it cannot.
    let offer = assess(&windows_disk("win")).map_err(|why| anyhow::anyhow!("{why}"))?;
    ensure!(
        offer.esp.number == 1
            && offer.kept.len() == 4
            && offer.placement.boot.size_bytes == 5 * GIB
            && offer.placement.root.start_bytes > 20 * GIB,
        "a Windows disk's offer: {offer:?}"
    );
    type Break = (&'static str, fn(&mut Disk), &'static str);
    let refusals: [Break; 13] = [
        (
            "no partition table",
            |d| d.partition_table = None,
            "no partition table",
        ),
        (
            "a DOS table",
            |d| d.partition_table = Some("dos".into()),
            "DOS partition table",
        ),
        (
            "no ESP",
            |d| d.partitions.retain(|p| p.type_id != ESP_TYPE),
            "no EFI system partition",
        ),
        (
            "an ESP that is not FAT",
            |d| d.partitions[0].filesystem = "ext4".into(),
            "not FAT",
        ),
        (
            "an ESP with no room",
            |d| d.esps[0].free_bytes = Some(8 * MIB),
            "8 MiB free",
        ),
        (
            "an ESP that cannot be read",
            |d| d.esps[0].readable = false,
            "could not be read",
        ),
        (
            "EFI/fedora",
            |d| d.esps[0].fedora_directory = true,
            "already holds EFI/fedora",
        ),
        (
            "no gap",
            |d| d.free_extents = vec![],
            "no unallocated space",
        ),
        (
            "a small gap",
            |d| d.free_extents = vec![extent(0, 20 * GIB)],
            "20 GiB",
        ),
        (
            "a root of another system",
            |d| {
                d.partitions.push(part(
                    5,
                    300 * GIB,
                    GIB,
                    ROOT_X86_64_TYPE,
                    "arch",
                    "ext4",
                    "bbbbbbbb-0000-4000-8000-000000000005",
                ));
            },
            "type ilex uses",
        ),
        (
            "a BIOS boot partition",
            |d| {
                d.partitions.push(part(
                    5,
                    300 * GIB,
                    MIB,
                    BIOS_BOOT_TYPE,
                    "",
                    "",
                    "bbbbbbbb-0000-4000-8000-000000000007",
                ));
            },
            "BIOS boot partition",
        ),
        (
            "an ilex installation",
            |d| {
                d.partitions.push(part(
                    5,
                    300 * GIB,
                    GIB,
                    ESP_TYPE,
                    "ilex-esp",
                    "vfat",
                    "bbbbbbbb-0000-4000-8000-000000000006",
                ));
            },
            "remove them with a partition tool",
        ),
        (
            "the partitions of an installation that did not finish",
            |d| {
                d.partitions.push(part(
                    5,
                    200 * GIB - 40 * GIB,
                    GIB,
                    XBOOTLDR_TYPE,
                    "ilex-boot",
                    "ext4",
                    "bbbbbbbb-0000-4000-8000-0000000000b0",
                ));
                d.partitions.push(part(
                    6,
                    200 * GIB - 39 * GIB,
                    30 * GIB,
                    ROOT_X86_64_TYPE,
                    "ilex-root",
                    "btrfs",
                    "bbbbbbbb-0000-4000-8000-0000000000b1",
                ));
            },
            "ilex-boot (1 GiB), ilex-root (30 GiB)",
        ),
    ];
    for (name, edit, says) in refusals {
        let mut broken = windows_disk("win");
        edit(&mut broken);
        let why = assess(&broken)
            .err()
            .with_context(|| format!("{name}: a disk beside which ilex cannot go was accepted"))?;
        ensure!(why.contains(says), "{name}: '{why}' does not say '{says}'");
    }
    // The plan for free-space mode: through the probe, with the offer.
    let mut probe = probe_of(table, &Edition::new("gnome")?)?;
    probe.disks.push(windows_disk("win"));
    let ok = check_request(&request_of(&|v| v["disk"] = json!("win"))?, &probe)
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    ensure!(ok.offer.is_some(), "free-space mode carries its offer");
    Ok(())
}

fn request_of(edit: &dyn Fn(&mut Value)) -> Result<PlanRequest> {
    let mut value = json!({
        "composition": "generic.gnome", "disk": "blank", "mode": "free-space",
        "administrator": "ilexmaint"
    });
    edit(&mut value);
    Ok(serde_json::from_value(value)?)
}
