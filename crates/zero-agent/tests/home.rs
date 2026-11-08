//! `project_id`: стабилен для одного каталога при разных записях пути.

use std::path::PathBuf;

use zero_agent::home::project_id;

#[test]
fn project_id_is_stable_and_path_independent() {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("home-project-id");
    let a = d.join("a");
    std::fs::create_dir_all(a.join("sub")).unwrap();
    std::fs::create_dir_all(d.join("b")).unwrap();
    let id = project_id(&a).unwrap();
    assert_eq!(id.len(), 16);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(project_id(&a.join("sub").join("..")).unwrap(), id);
    assert_ne!(project_id(&d.join("b")).unwrap(), id);
    assert!(project_id(&d.join("missing")).is_err());
}
