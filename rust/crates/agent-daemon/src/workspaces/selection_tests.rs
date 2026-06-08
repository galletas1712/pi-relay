use super::*;

#[test]
fn all_keeps_every_workspace_in_project_order() {
    let project = vec![
        ProjectWorkspace::git("repo-a", "https://example.test/a.git", "main"),
        ProjectWorkspace::local("docs", "/srv/docs"),
    ];
    let resolved = WorkspaceSelection::All
        .resolve(&project)
        .expect("resolve all");
    assert_eq!(
        resolved,
        vec![
            SelectedWorkspace {
                workspace: project[0].clone(),
                branch_override: None,
            },
            SelectedWorkspace {
                workspace: project[1].clone(),
                branch_override: None,
            },
        ]
    );
}

#[test]
fn subset_filters_and_preserves_project_order() {
    let project = vec![
        ProjectWorkspace::git("repo-a", "https://example.test/a.git", "main"),
        ProjectWorkspace::local("docs", "/srv/docs"),
        ProjectWorkspace::git("repo-b", "https://example.test/b.git", "main"),
    ];
    // Requested out of order; resolution must follow project-declared order.
    let selection = WorkspaceSelection::Subset(vec![
        RequestedWorkspace {
            workspace_dir: "repo-b".to_string(),
            branch: Some("feature".to_string()),
        },
        RequestedWorkspace {
            workspace_dir: "repo-a".to_string(),
            branch: None,
        },
    ]);
    let resolved = selection.resolve(&project).expect("resolve subset");
    assert_eq!(
        resolved,
        vec![
            SelectedWorkspace {
                workspace: project[0].clone(),
                branch_override: None,
            },
            SelectedWorkspace {
                workspace: project[2].clone(),
                branch_override: Some("feature".to_string()),
            },
        ]
    );
}

#[test]
fn rejects_invalid_requests() {
    let project = vec![
        ProjectWorkspace::git("repo-a", "https://example.test/a.git", "main"),
        ProjectWorkspace::local("docs", "/srv/docs"),
    ];

    let empty = WorkspaceSelection::Subset(Vec::new());
    assert!(empty.resolve(&project).is_err());

    let unknown = WorkspaceSelection::Subset(vec![RequestedWorkspace {
        workspace_dir: "missing".to_string(),
        branch: None,
    }]);
    assert!(unknown.resolve(&project).is_err());

    let duplicate = WorkspaceSelection::Subset(vec![
        RequestedWorkspace {
            workspace_dir: "repo-a".to_string(),
            branch: None,
        },
        RequestedWorkspace {
            workspace_dir: "repo-a".to_string(),
            branch: None,
        },
    ]);
    assert!(duplicate.resolve(&project).is_err());

    let branch_on_local = WorkspaceSelection::Subset(vec![RequestedWorkspace {
        workspace_dir: "docs".to_string(),
        branch: Some("feature".to_string()),
    }]);
    assert!(branch_on_local.resolve(&project).is_err());
}
