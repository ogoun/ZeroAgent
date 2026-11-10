//! Метки `za_core::shell::label` (крейт `za-shell`, без зависимостей) совпадают с
//! константами протокола `za_protocol::irreversible` (D81, D99).

use za_core::shell::{self, label};
use za_protocol::irreversible as proto;

#[test]
fn shell_labels_match_protocol() {
    for (a, b) in [
        (label::DELETE, proto::DELETE),
        (label::GIT_RESET, proto::GIT_RESET),
        (label::GIT_FORCE_PUSH, proto::GIT_FORCE_PUSH),
        (label::PUBLISH, proto::PUBLISH),
        (label::INFRA, proto::INFRA),
        (label::SQL_DROP, proto::SQL_DROP),
        (label::OVERWRITE, proto::OVERWRITE),
        (label::UNPARSED, proto::UNPARSED),
        (label::EXEC_UNTRUSTED, proto::EXEC_UNTRUSTED),
        (label::GIT_CONFIG_EXEC, proto::GIT_CONFIG_EXEC),
    ] {
        assert_eq!(a, b);
    }
}

#[test]
fn reexport_is_usable_through_za_core() {
    let segs = shell::segments("cargo test", shell::Dialect::Bash);
    assert!(shell::is_simple(&segs));
}
