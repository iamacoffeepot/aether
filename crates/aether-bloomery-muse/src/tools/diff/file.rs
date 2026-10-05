//! One changed file's diff block.

use aether_bloomery_kinds::Refusal;
use aether_bloomery_program::{Async, Env};

use super::DIFF_MAX_FILE_BYTES;
use super::hunks::{FileChange, binary, header, hunks, render_hunk, too_different, too_large};
use super::myers::edits;
use super::walk::{Changed, Leaf};
use crate::tools::view::Lines;

/// What showing one file read and whether it fit the output budget.
pub(super) struct Shown {
    pub(super) fit: bool,
    pub(super) read_bytes: usize,
}

/// The diff block for `change` pushed into `lines`.
///
/// # Errors
///
/// The [`Refusal`] of a file the store cannot give.
pub(super) async fn block(
    env: &mut Env<Async>,
    change: &Changed,
    max_edits: usize,
    lines: &mut Lines,
) -> Result<Shown, Refusal> {
    let file_change = match (&change.old, &change.new) {
        (None, Some(_)) => FileChange::Added,
        (Some(_), None) => FileChange::Removed,
        (Some(Leaf::File { mode: old_mode, .. }), Some(Leaf::File { mode: new_mode, .. })) => {
            FileChange::Modified { modes: Some((*old_mode, *new_mode)) }
        }
        _ => FileChange::Modified { modes: None },
    };
    let fits_header = header(&change.path, file_change, lines);
    if !fits_header {
        return Ok(Shown { fit: false, read_bytes: 0 });
    }
    if let (Some(Leaf::File { blob: old_blob, .. }), Some(Leaf::File { blob: new_blob, .. })) =
        (&change.old, &change.new)
    {
        let same_blob = old_blob == new_blob;
        if same_blob {
            return Ok(Shown { fit: true, read_bytes: 0 });
        }
    }
    let mut read_bytes = 0;
    let old_bytes = match &change.old {
        None => Vec::new(),
        Some(Leaf::File { blob, .. }) => {
            let payload = env.read_payload(blob.erase()).await?;
            read_bytes += payload.len();
            payload
        }
        Some(Leaf::Symlink(target)) => target.as_str().as_bytes().to_vec(),
    };
    let new_bytes = match &change.new {
        None => Vec::new(),
        Some(Leaf::File { blob, .. }) => {
            let payload = env.read_payload(blob.erase()).await?;
            read_bytes += payload.len();
            payload
        }
        Some(Leaf::Symlink(target)) => target.as_str().as_bytes().to_vec(),
    };
    let old_too_large = old_bytes.len() > DIFF_MAX_FILE_BYTES;
    let new_too_large = new_bytes.len() > DIFF_MAX_FILE_BYTES;
    let over_cap = old_too_large || new_too_large;
    if over_cap {
        let fit = too_large(lines);
        return Ok(Shown { fit, read_bytes });
    }
    let Ok(old_text) = str::from_utf8(&old_bytes) else {
        let fit = binary(&change.path, lines);
        return Ok(Shown { fit, read_bytes });
    };
    let Ok(new_text) = str::from_utf8(&new_bytes) else {
        let fit = binary(&change.path, lines);
        return Ok(Shown { fit, read_bytes });
    };
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let Some(script) = edits(&old_lines, &new_lines, max_edits) else {
        let fit = too_different(old_lines.len(), new_lines.len(), lines);
        return Ok(Shown { fit, read_bytes });
    };
    let grouped = hunks(&script);
    let empty = grouped.is_empty();
    if empty {
        let fit = lines.push("@@ only line endings or the final newline differ @@");
        return Ok(Shown { fit, read_bytes });
    }
    let mut fit = true;
    for hunk in &grouped {
        let pushed = render_hunk(hunk, &script, &old_lines, &new_lines, lines);
        if !pushed {
            fit = false;
            break;
        }
    }
    Ok(Shown { fit, read_bytes })
}
