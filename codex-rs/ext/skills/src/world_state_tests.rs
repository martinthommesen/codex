use super::*;
use pretty_assertions::assert_eq;

#[test]
fn disabled_sections_retract_listed_catalogs_once() {
    let listed =
        json!({ "body": "\n## Skills\n- demo: Demo skill\n", "includeInstructions": true });
    let empty = json!({ "body": null, "includeInstructions": true });
    let disabled = json!({ "disabled": true });

    let renders = disabled_skills_world_state_sections()
        .iter()
        .map(|section| {
            let previous = if section.id() == CLOUD_SKILLS_WORLD_STATE_ID {
                PreviousWorldStateSection::Known(&empty)
            } else {
                PreviousWorldStateSection::Known(&listed)
            };
            let (snapshot, fragment) = section.render_diff(previous);
            (section.id(), snapshot, fragment.is_some())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        renders,
        vec![
            (HOST_SKILLS_WORLD_STATE_ID, Some(disabled.clone()), true),
            (SKILLS_WORLD_STATE_ID, Some(disabled.clone()), false),
            (CLOUD_SKILLS_WORLD_STATE_ID, Some(disabled.clone()), false),
        ]
    );

    // Fresh threads and catalogs that were already retracted stay silent.
    for section in disabled_skills_world_state_sections() {
        assert_eq!(
            section.render_diff(PreviousWorldStateSection::Absent),
            (Some(disabled.clone()), None)
        );
        assert_eq!(
            section.render_diff(PreviousWorldStateSection::Known(&disabled)),
            (Some(disabled.clone()), None)
        );
    }
}
