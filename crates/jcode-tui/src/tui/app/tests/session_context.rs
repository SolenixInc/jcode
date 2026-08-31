#[cfg(unix)]
fn write_session_context_test_script(
    dir: &std::path::Path,
    name: &str,
    body: &str,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(name);
    std::fs::write(&path, body).expect("write session_context test hook");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod session_context test hook");
    path
}

#[cfg(unix)]
struct SessionContextTestEnv {
    previous_hook: Option<std::ffi::OsString>,
    previous_timeout: Option<std::ffi::OsString>,
}

#[cfg(unix)]
impl SessionContextTestEnv {
    fn new(command: &std::path::Path) -> Self {
        let guard = Self {
            previous_hook: std::env::var_os("JCODE_HOOK_SESSION_CONTEXT"),
            previous_timeout: std::env::var_os("JCODE_HOOK_SESSION_CONTEXT_TIMEOUT_MS"),
        };
        crate::env::set_var("JCODE_HOOK_SESSION_CONTEXT", command);
        crate::env::set_var("JCODE_HOOK_SESSION_CONTEXT_TIMEOUT_MS", "5000");
        crate::config::invalidate_config_cache();
        guard
    }
}

#[cfg(unix)]
impl Drop for SessionContextTestEnv {
    fn drop(&mut self) {
        match self.previous_hook.take() {
            Some(value) => crate::env::set_var("JCODE_HOOK_SESSION_CONTEXT", value),
            None => crate::env::remove_var("JCODE_HOOK_SESSION_CONTEXT"),
        }
        match self.previous_timeout.take() {
            Some(value) => crate::env::set_var("JCODE_HOOK_SESSION_CONTEXT_TIMEOUT_MS", value),
            None => crate::env::remove_var("JCODE_HOOK_SESSION_CONTEXT_TIMEOUT_MS"),
        }
        crate::config::invalidate_config_cache();
    }
}

#[cfg(unix)]
#[test]
fn local_tui_create_runs_session_context_hook_and_persists_provider_context() {
    with_temp_jcode_home(|| {
        let hook_dir = tempfile::TempDir::new().expect("hook temp dir");
        let hook = write_session_context_test_script(
            hook_dir.path(),
            "create.sh",
            "#!/bin/sh\nprintf 'local tui create context'\n",
        );
        let _hook_env = SessionContextTestEnv::new(&hook);

        let app = create_test_app();
        let context_message = app
            .session
            .messages
            .iter()
            .find(|message| {
                message.display_role == Some(crate::session::StoredDisplayRole::System)
                    && message.content.iter().any(|block| {
                        matches!(
                            block,
                            ContentBlock::Text { text, .. }
                                if text.contains("local tui create context")
                        )
                    })
            })
            .expect("local TUI create must append hook context");
        assert!(context_message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Text { text, .. }
                    if text.contains("<system-reminder>")
                        && text.contains("local tui create context")
            )
        }));

        let restored = crate::session::Session::load(&app.session.id)
            .expect("create-time hook context must be persisted before a visible turn");
        assert!(restored.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { text, .. }
                        if text.contains("local tui create context")
                )
            })
        }));
    });
}

#[cfg(unix)]
#[test]
fn local_tui_restore_runs_session_context_hook_for_the_restored_session() {
    with_temp_jcode_home(|| {
        let mut target = crate::session::Session::create(None, None);
        target.ensure_initial_session_context_message();
        target.add_message(crate::message::Role::User, vec![ContentBlock::Text {
            text: "existing conversation".to_string(),
            cache_control: None,
        }]);
        target.save().expect("persist restore target");

        let mut app = create_test_app();
        let hook_dir = tempfile::TempDir::new().expect("hook temp dir");
        let hook = write_session_context_test_script(
            hook_dir.path(),
            "restore.sh",
            "#!/bin/sh\nprintf 'local tui %s:%s' \"$JCODE_HOOK_SOURCE\" \"$JCODE_HOOK_SESSION_ID\"\n",
        );
        let _hook_env = SessionContextTestEnv::new(&hook);

        app.restore_session(&target.id);

        let restored = crate::session::Session::load(&target.id).expect("load restored session");
        let hook_messages = restored
            .messages
            .iter()
            .filter(|message| {
                message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text { text, .. }
                            if text.contains("<!-- jcode:session_context -->")
                    )
                })
            })
            .count();
        assert_eq!(hook_messages, 1, "restore must persist one hook context message");
        assert!(restored.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { text, .. }
                        if text.contains("local tui resume:")
                            && text.contains(&target.id)
                )
            })
        }));
    });
}

#[cfg(unix)]
#[test]
fn local_tui_repeated_restore_replaces_session_context_without_duplicates() {
    with_temp_jcode_home(|| {
        let mut target = crate::session::Session::create(None, None);
        target.ensure_initial_session_context_message();
        target.add_message(crate::message::Role::User, vec![ContentBlock::Text {
            text: "existing conversation".to_string(),
            cache_control: None,
        }]);
        target.save().expect("persist restore target");

        let mut app = create_test_app();
        let hook_dir = tempfile::TempDir::new().expect("hook temp dir");
        let invocations = hook_dir.path().join("invocations");
        let hook = write_session_context_test_script(
            hook_dir.path(),
            "repeated-restore.sh",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$JCODE_HOOK_SESSION_ID\" >> '{}'\nprintf 'local tui repeated restore context'\n",
                invocations.display()
            ),
        );
        let _hook_env = SessionContextTestEnv::new(&hook);

        app.restore_session(&target.id);
        app.restore_session(&target.id);

        let restored = crate::session::Session::load(&target.id).expect("load restored session");
        let hook_messages = restored
            .messages
            .iter()
            .filter(|message| {
                message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text { text, .. }
                            if text.contains("<!-- jcode:session_context -->")
                    )
                })
            })
            .count();
        assert_eq!(hook_messages, 1, "repeated restore must replace one hook context message");
        assert_eq!(
            std::fs::read_to_string(invocations)
                .expect("hook invocation log")
                .lines()
                .count(),
            2,
            "each restore must run the hook for the restored session"
        );
    });
}

#[cfg(unix)]
#[test]
fn local_tui_concurrent_session_context_injections_stay_session_scoped() {
    with_temp_jcode_home(|| {
        let hook_dir = tempfile::TempDir::new().expect("hook temp dir");
        let invocations = hook_dir.path().join("invocations");
        let hook = write_session_context_test_script(
            hook_dir.path(),
            "concurrent.sh",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$JCODE_HOOK_SESSION_ID\" >> '{}'\nprintf 'context for %s' \"$JCODE_HOOK_SESSION_ID\"\n",
                invocations.display()
            ),
        );
        let _hook_env = SessionContextTestEnv::new(&hook);

        let first = create_test_app();
        let second = create_test_app();
        let first_context = first
            .session
            .messages
            .iter()
            .find_map(|message| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text, .. }
                        if text.contains("<!-- jcode:session_context -->") => Some(text.clone()),
                    _ => None,
                })
            })
            .expect("first session context");
        let second_context = second
            .session
            .messages
            .iter()
            .find_map(|message| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text, .. }
                        if text.contains("<!-- jcode:session_context -->") => Some(text.clone()),
                    _ => None,
                })
            })
            .expect("second session context");

        assert!(first_context.contains(&first.session.id));
        assert!(!first_context.contains(&second.session.id));
        assert!(second_context.contains(&second.session.id));
        assert!(!second_context.contains(&first.session.id));
        assert_eq!(
            std::fs::read_to_string(invocations)
                .expect("hook invocation log")
                .lines()
                .count(),
            2
        );
    });
}

#[cfg(unix)]
#[test]
fn local_tui_failed_session_context_hook_blocks_manual_compaction() {
    with_temp_jcode_home(|| {
        let hook_dir = tempfile::TempDir::new().expect("hook temp dir");
        let hook = write_session_context_test_script(
            hook_dir.path(),
            "failing.sh",
            "#!/bin/sh\nprintf 'local tui hook unavailable' >&2\nexit 9\n",
        );
        let _hook_env = SessionContextTestEnv::new(&hook);
        let mut app = create_test_app();

        super::commands::handle_config_command(&mut app, "/compact");

        let notice = app.display_messages().last().expect("compaction notice");
        assert!(notice.content.contains("Session context hook blocked startup"));
        assert!(notice.content.contains("local tui hook unavailable"));
    });
}

#[cfg(unix)]
#[test]
fn local_tui_failed_session_context_hook_rejects_user_submission_before_staging_turn() {
    with_temp_jcode_home(|| {
        let hook_dir = tempfile::TempDir::new().expect("hook temp dir");
        let hook = write_session_context_test_script(
            hook_dir.path(),
            "failing-submit.sh",
            "#!/bin/sh\nprintf 'local tui hook unavailable' >&2\nexit 9\n",
        );
        let _hook_env = SessionContextTestEnv::new(&hook);
        let mut app = create_test_app();

        app.handle_key(crossterm::event::KeyCode::Char('h'), crossterm::event::KeyModifiers::empty())
            .expect("type prompt");
        app.handle_key(crossterm::event::KeyCode::Enter, crossterm::event::KeyModifiers::empty())
            .expect("submit prompt");

        assert_eq!(app.input(), "h");
        assert!(!app.is_processing());
        let notice = app.display_messages().last().expect("hook blocker notice");
        assert!(notice.content.contains("Session context hook blocked startup"));
        assert!(notice.content.contains("local tui hook unavailable"));
    });
}

#[cfg(unix)]
#[test]
fn local_tui_failed_session_context_hook_blocks_local_transfer_prepare() {
    with_temp_jcode_home(|| {
        let hook_dir = tempfile::TempDir::new().expect("hook temp dir");
        let hook = write_session_context_test_script(
            hook_dir.path(),
            "failing-transfer.sh",
            "#!/bin/sh\nprintf 'local tui hook unavailable' >&2\nexit 9\n",
        );
        let _hook_env = SessionContextTestEnv::new(&hook);
        let mut app = create_test_app();
        app.session.compaction = Some(crate::session::StoredCompactionState {
            summary_text: "already compacted".to_string(),
            openai_encrypted_content: None,
            covers_up_to_turn: 0,
            original_turn_count: 0,
            compacted_count: app.session.messages.len(),
        });

        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let result = runtime.block_on(async {
            super::commands::start_local_transfer_prepare(&mut app)
        });

        let error = result.expect_err("a failed session_context hook must block local transfer");
        assert!(error.to_string().contains("Session context hook blocked startup"));
        assert!(app.pending_local_transfer.is_none());
    });
}

#[cfg(unix)]
#[test]
fn local_tui_recovery_runs_a_fresh_session_context_hook_without_duplicates() {
    with_temp_jcode_home(|| {
        let hook_dir = tempfile::TempDir::new().expect("hook temp dir");
        let invocations = hook_dir.path().join("invocations");
        let hook = write_session_context_test_script(
            hook_dir.path(),
            "recovery.sh",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$JCODE_HOOK_SOURCE\" >> '{}'\nprintf 'local tui %s context' \"$JCODE_HOOK_SOURCE\"\n",
                invocations.display()
            ),
        );
        let _hook_env = SessionContextTestEnv::new(&hook);
        let mut app = create_test_app();
        let original_id = app.session.id.clone();
        app.session.add_message(Role::User, vec![ContentBlock::Text {
            text: "conversation to recover".to_string(),
            cache_control: None,
        }]);
        app.session.save().expect("persist recovery source");

        app.recover_session_without_tools();
        let first_recovery_id = app.session.id.clone();
        app.recover_session_without_tools();

        let recovered = crate::session::Session::load(&app.session.id).expect("load recovery session");
        let hook_messages = recovered
            .messages
            .iter()
            .filter(|message| {
                message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text { text, .. }
                            if text.contains("<!-- jcode:session_context -->")
                    )
                })
            })
            .count();
        assert_ne!(app.session.id, original_id);
        assert_ne!(app.session.id, first_recovery_id);
        assert_eq!(hook_messages, 1, "recovery must retain one fresh hook context message");
        assert_eq!(
            std::fs::read_to_string(invocations)
                .expect("hook invocation log")
                .lines()
                .count(),
            3,
            "create and repeated recovery must each run the hook"
        );
    });
}

#[cfg(unix)]
#[test]
fn local_tui_clear_runs_session_context_hook_for_the_replacement_session() {
    with_temp_jcode_home(|| {
        let hook_dir = tempfile::TempDir::new().expect("hook temp dir");
        let invocations = hook_dir.path().join("invocations");
        let hook = write_session_context_test_script(
            hook_dir.path(),
            "clear.sh",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$JCODE_HOOK_SOURCE\" >> '{}'\nprintf 'local tui %s context' \"$JCODE_HOOK_SOURCE\"\n",
                invocations.display()
            ),
        );
        let _hook_env = SessionContextTestEnv::new(&hook);
        let mut app = create_test_app();
        let original_id = app.session.id.clone();

        super::commands_review::reset_current_session(&mut app);

        assert_ne!(app.session.id, original_id);
        assert!(app.session.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { text, .. }
                        if text.contains("local tui create context")
                )
            })
        }));
        assert_eq!(
            std::fs::read_to_string(invocations)
                .expect("hook invocation log")
                .lines()
                .count(),
            2,
            "initial create and /clear replacement must each run the hook"
        );
    });
}

#[cfg(unix)]
#[test]
fn local_tui_fix_does_not_bypass_a_failed_session_context_hook() {
    with_temp_jcode_home(|| {
        let hook_dir = tempfile::TempDir::new().expect("hook temp dir");
        let hook = write_session_context_test_script(
            hook_dir.path(),
            "failing-fix.sh",
            "#!/bin/sh\nprintf 'local tui hook unavailable' >&2\nexit 9\n",
        );
        let _hook_env = SessionContextTestEnv::new(&hook);
        let mut app = create_test_app();

        app.run_fix_command();

        let notice = app.display_messages().last().expect("fix notice");
        assert!(notice.content.contains("Session context hook blocked startup"));
        assert!(notice.content.contains("local tui hook unavailable"));
    });
}
