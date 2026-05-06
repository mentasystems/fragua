//! Fab pack bundling — write every output file for a project to a
//! directory. File naming follows the JLC/KiCad convention so the pack
//! drops straight into a fab portal.

use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};

use pcb_core::Board;

use crate::gerber::Side;
use crate::{bom, excellon, gerber, pick_place};

/// Write the full fab pack for `board` into `out_dir`. Returns the list
/// of files created in the order they were written.
pub fn write_fab_pack(
    board: &Board,
    project_name: &str,
    out_dir: &Path,
) -> io::Result<Vec<PathBuf>> {
    fs::create_dir_all(out_dir)?;
    let mut paths = Vec::with_capacity(9);

    let stem = sanitize(project_name);

    paths.push(write_to(out_dir, &format!("{stem}-F_Cu.gbr"), |w| {
        gerber::write_copper(board, Side::Top, w)
    })?);
    paths.push(write_to(out_dir, &format!("{stem}-B_Cu.gbr"), |w| {
        gerber::write_copper(board, Side::Bottom, w)
    })?);
    paths.push(write_to(out_dir, &format!("{stem}-F_Mask.gbr"), |w| {
        gerber::write_mask(board, Side::Top, w)
    })?);
    paths.push(write_to(out_dir, &format!("{stem}-B_Mask.gbr"), |w| {
        gerber::write_mask(board, Side::Bottom, w)
    })?);
    paths.push(write_to(out_dir, &format!("{stem}-F_SilkS.gbr"), |w| {
        gerber::write_silk(board, Side::Top, w)
    })?);
    paths.push(write_to(out_dir, &format!("{stem}-B_SilkS.gbr"), |w| {
        gerber::write_silk(board, Side::Bottom, w)
    })?);
    paths.push(write_to(out_dir, &format!("{stem}-Edge_Cuts.gbr"), |w| {
        gerber::write_edge_cuts(board, w)
    })?);
    paths.push(write_to(out_dir, &format!("{stem}-PTH.drl"), |w| {
        excellon::write(board, true, w)
    })?);
    paths.push(write_to(out_dir, &format!("{stem}-NPTH.drl"), |w| {
        excellon::write(board, false, w)
    })?);
    paths.push(write_to(out_dir, &format!("{stem}-bom.csv"), |w| {
        bom::write(board, w)
    })?);
    paths.push(write_to(out_dir, &format!("{stem}-pos.csv"), |w| {
        pick_place::write(board, w)
    })?);

    Ok(paths)
}

fn write_to<F>(dir: &Path, name: &str, body: F) -> io::Result<PathBuf>
where
    F: FnOnce(&mut BufWriter<File>) -> io::Result<()>,
{
    let path = dir.join(name);
    let mut w = BufWriter::new(File::create(&path)?);
    body(&mut w)?;
    w.into_inner()
        .map_err(|e| io::Error::other(format!("flush {name}: {e}")))?
        .sync_all()?;
    Ok(path)
}

/// Replace anything outside `[A-Za-z0-9._-]` with `_`. Fab portals
/// occasionally choke on spaces or unicode in filenames, and we use
/// the project name verbatim as the filename stem.
fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("untitled");
    }
    out
}
