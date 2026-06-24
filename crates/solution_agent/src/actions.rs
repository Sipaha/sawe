use gpui::actions;

actions!(
    solution_agent,
    [
        /// Create a new AI session in the current solution.
        NewSession,
        /// Focus the solution sessions navigator panel.
        FocusNavigator,
        /// Focus the active session view.
        FocusActiveSession,
        /// Cycle through sessions in the current solution.
        CycleSession,
        /// Duplicate the active session.
        DuplicateSession,
        /// Close the active session.
        CloseSession,
        /// Restart the agent subprocess for the active session.
        RestartAgent,
        /// Open or focus the in-session find bar (Ctrl+F over chat history).
        FindInSession,
        /// Move to the next match in the in-session find bar.
        FindNextMatch,
        /// Move to the previous match in the in-session find bar.
        FindPreviousMatch,
        /// Close the in-session find bar.
        FindClose,
        /// Cancel the agent's in-flight response in the active session.
        StopResponse,
        /// Paste only the text portion of the clipboard into the
        /// compose editor — skips images / file paths / any rich
        /// content the default Ctrl-V would have wrapped into a
        /// pending image. Bound to Ctrl/Cmd-Shift-V.
        PasteWithoutFormatting,
    ]
);
