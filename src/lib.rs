//! ElevenLabs on Pointman's machines (James, Pointman thread 10399 [15505]; the brief, thread 15508): a thin
//! wrapper of ElevenLabs' official CLI. `cli` finds and runs it, `install` puts ElevenLabs' own build in the
//! extension's state folder where there's none, and `args` checks what `elevenlabs.run` passes through.

pub mod args;
pub mod cli;
pub mod install;
pub mod words;
