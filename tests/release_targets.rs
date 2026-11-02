//! Guards against publishing binaries for platforms macman can't yet support.
//! See docs/PLATFORMS.md.

const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");

/// The `"target"` values in the workflow's matrix.
fn targets() -> Vec<&'static str> {
    RELEASE_WORKFLOW
        .split("\"target\":\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap())
        .collect()
}

#[test]
fn release_matrix_is_explicit() {
    assert_eq!(
        targets(),
        [
            "x86_64-unknown-linux-gnu",
            "i686-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
        ]
    );
}

#[test]
fn deferred_targets_are_not_released() {
    for deferred in [
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "i686-pc-windows-msvc",
    ] {
        assert!(!targets().contains(&deferred), "{deferred}");
    }
}

#[test]
fn deferred_targets_stay_documented() {
    let docs = include_str!("../docs/PLATFORMS.md");
    for row in [
        "| Linux x86-64 | Yes | Yes |",
        "| Windows x86-64 | Yes | Yes |",
        "| Linux ARM64 | No | **No, deferred** |",
        "| macOS x86-64 | No | **No, deferred** |",
        "| Windows i686 | No | **No, deferred** |",
    ] {
        assert!(docs.contains(row), "{row}");
    }
}
