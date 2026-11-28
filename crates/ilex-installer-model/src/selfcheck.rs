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
            "free space",
            ErrorCode::UnsupportedMode,
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
    Ok(
        "table rules, editions, detection, selection, requests, answers, secrets, stages, finish, recovery key"
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
