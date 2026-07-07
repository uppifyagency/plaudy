use clap::Parser;

#[derive(Parser, Debug, Clone, Default)]
#[command(name = "plaudy", about = "Plaudy - Local AI voice notes & dictation")]
pub struct CliArgs {
    /// Start with the main window hidden
    #[arg(long)]
    pub start_hidden: bool,

    /// Disable the system tray icon
    #[arg(long)]
    pub no_tray: bool,

    /// Toggle transcription on/off (sent to running instance)
    #[arg(long)]
    pub toggle_transcription: bool,

    /// Toggle transcription with post-processing on/off (sent to running instance)
    #[arg(long)]
    pub toggle_post_process: bool,

    /// Cancel the current operation (sent to running instance)
    #[arg(long)]
    pub cancel: bool,

    /// Toggle a long-form recording session on/off (sent to running instance)
    #[arg(long)]
    pub toggle_session: bool,

    /// Toggle a long-form SYSTEM-AUDIO recording session on/off (sent to running instance)
    #[arg(long)]
    pub toggle_system_session: bool,

    /// Toggle a long-form MEETING session on/off — captures mic + system audio as two streams
    /// merged into one speaker-attributed transcript (sent to running instance). This is the
    /// menu-bar "graffetta" action, exposed as a flag for scripting and headless testing.
    #[arg(long)]
    pub toggle_meeting: bool,

    /// Re-run transcription + diarization for one history entry, by id (sent to running
    /// instance). Maintenance/repair: rebuilds a lost speaker timeline (e.g. a row recovered by
    /// an older flat retry). Meeting/System rows are re-diarized to "Speaker N"; "Me" is not
    /// recoverable from a mixed WAV (see commands::history::retranscribe_for_retry).
    #[arg(long, value_name = "ID")]
    pub rediarize: Option<i64>,

    /// Enable debug mode with verbose logging
    #[arg(long)]
    pub debug: bool,
}
