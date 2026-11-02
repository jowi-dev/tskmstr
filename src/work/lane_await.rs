//! Finishing an unattended interactive lane on its done marker (GitHub
//! issue #184).
//!
//! `tm work run --unattended` (the autopilot dispatcher's launch) runs its
//! lane interactively in the ticket session's `work` window, where claude
//! sits at its prompt once it is done instead of exiting. The lane's prompt
//! ends with [`lane_completion_section`], telling the agent to write a done
//! marker once its pull request is open.

use std::path::Path;

use crate::work::signal::temporary_name;

/// The section tm appends to an unattended interactive lane's prompt: tm
/// waits for `marker` ([`crate::work::run::PreparedRun::done_marker`]),
/// written last under a temporary name and renamed into place.
pub fn lane_completion_section(marker: &Path) -> String {
    let tmp = temporary_name(marker);
    format!(
        "\n\n## Completion signal\n\n\
         tm is waiting for `{file}` to know this lane is done. Once the pull \
         request is open, write `{{\"succeeded\": true}}` to `{tmp}`, or \
         `{{\"succeeded\": false}}` if you are stopping without a pull \
         request (blocked, or you cannot finish), then rename it into place \
         with `mv '{tmp}' '{file}'`, so tm never reads a half-written file. \
         Then stop and leave this session open: a human may attach to it.\n",
        file = marker.display(),
        tmp = tmp.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lane_completion_section_names_the_marker_and_its_temporary_name() {
        let marker = Path::new("/state/proj-1-20260806-090503.done");
        let section = lane_completion_section(marker);
        assert!(section.starts_with("\n\n## Completion signal\n\n"));
        assert!(section.contains("`/state/proj-1-20260806-090503.done`"));
        assert!(section.contains(
            "mv '/state/proj-1-20260806-090503.done.tmp' '/state/proj-1-20260806-090503.done'"
        ));
        assert!(section.contains(r#"{"succeeded": true}"#));
        assert!(section.contains(r#"{"succeeded": false}"#));
        assert!(section.contains("leave this session open"));
    }
}
