use std::fs::File;
use std::io::prelude::*;
use std::sync::Arc;

use crate::api::sector_builder::errors::*;
use crate::api::sector_builder::metadata::StagedSectorMetadata;
use crate::api::sector_builder::pieces::get_piece_padding;
use crate::api::sector_builder::pieces::sum_piece_lengths;
use crate::api::sector_builder::state::StagedState;
use crate::api::sector_builder::*;
use crate::error;
use sector_base::api::bytes_amount::UnpaddedBytesAmount;
use sector_base::api::sector_store::SectorManager;

pub fn add_piece(
    sector_store: &Arc<WrappedSectorStore>,
    mut staged_state: &mut StagedState,
    piece_key: String,
    piece_bytes_amount: u64,
    piece_path: String,
) -> error::Result<SectorId> {
    let sector_mgr = sector_store.inner.manager();
    let sector_max = sector_store
        .inner
        .sector_config()
        .max_unsealed_bytes_per_sector();

    let piece_bytes_len = UnpaddedBytesAmount(piece_bytes_amount);

    let opt_dest_sector_id = {
        let candidates: Vec<StagedSectorMetadata> = staged_state
            .sectors
            .iter()
            .filter(|(_, v)| v.seal_status == SealStatus::Pending)
            .map(|(_, v)| (*v).clone())
            .collect();

        compute_destination_sector_id(&candidates[..], sector_max, piece_bytes_len)?
    };

    let dest_sector_id = opt_dest_sector_id
        .ok_or(())
        .or_else(|_| provision_new_staged_sector(sector_mgr, &mut staged_state))?;

    if let Some(s) = staged_state.sectors.get_mut(&dest_sector_id) {
        let file = File::open(piece_path)?;

        let sector_length = sum_piece_lengths(s.pieces.iter());
        let (left_padding, right_padding) = get_piece_padding(sector_length, UnpaddedBytesAmount(piece_bytes_amount));

         let left_padding_vec = vec![0; left_padding.into()];
         let left_padding_slice = &left_padding_vec[..];
         let right_padding_vec = vec![0; right_padding.into()];
         let right_padding_slice = &right_padding_vec[..];
         let mut chain = left_padding_slice.chain(file).chain(right_padding_slice);
         let expected_num_bytes_written = left_padding + piece_bytes_len + right_padding;

        sector_store
            .inner
            .manager()
            .write_and_preprocess(&s.sector_access, &mut chain)
            .map_err(Into::into)
            .and_then(|num_bytes_written| {
                if num_bytes_written != expected_num_bytes_written {
                    Err(
                        err_inc_write(u64::from(num_bytes_written), u64::from(expected_num_bytes_written))
                            .into(),
                    )
                } else {
                    Ok(s.sector_id)
                }
            })
            .map(|sector_id| {
                s.pieces.push(metadata::PieceMetadata {
                    piece_key,
                    num_bytes: piece_bytes_len,
                    comm_p: None,
                });

                sector_id
            })
    } else {
        Err(err_unrecov("unable to retrieve sector from state-map").into())
    }
}

// Given a list of staged sectors which are accepting data, return the
// first staged sector into which the bytes will fit.
fn compute_destination_sector_id(
    candidate_sectors: &[StagedSectorMetadata],
    max_bytes_per_sector: UnpaddedBytesAmount,
    num_bytes_in_piece: UnpaddedBytesAmount,
) -> error::Result<Option<SectorId>> {
    if num_bytes_in_piece > max_bytes_per_sector {
        Err(err_overflow(num_bytes_in_piece.into(), max_bytes_per_sector.into()).into())
    } else {
        Ok(candidate_sectors
            .iter()
            .find(move |staged_sector| {
                let sector_length = sum_piece_lengths(staged_sector.pieces.iter());
                let (left_padding, right_padding) = get_piece_padding(sector_length, num_bytes_in_piece);
                (sector_length + left_padding + num_bytes_in_piece + right_padding)
                    <= max_bytes_per_sector
            })
            .map(|x| x.sector_id))
    }
}

// Provisions a new staged sector and returns its sector_id. Not a pure
// function; creates a sector access (likely a file), increments the sector id
// nonce, and mutates the StagedState.
fn provision_new_staged_sector(
    sector_manager: &SectorManager,
    staged_state: &mut StagedState,
) -> error::Result<SectorId> {
    let sector_id = {
        let n = &mut staged_state.sector_id_nonce;
        *n += 1;
        *n
    };

    let access = sector_manager.new_staging_sector_access()?;

    let meta = StagedSectorMetadata {
        pieces: Default::default(),
        sector_access: access.clone(),
        sector_id,
        seal_status: SealStatus::Pending,
    };

    staged_state.sectors.insert(meta.sector_id, meta.clone());

    Ok(sector_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::sector_builder::metadata::PieceMetadata;

    #[test]
    fn test_alpha() {
        let mut sealed_sector_a: StagedSectorMetadata = Default::default();

        sealed_sector_a.pieces.push(PieceMetadata {
            piece_key: String::from("x"),
            num_bytes: UnpaddedBytesAmount(128),
            comm_p: None,
        });

        sealed_sector_a.pieces.push(PieceMetadata {
            piece_key: String::from("x"),
            num_bytes: UnpaddedBytesAmount(128),
            comm_p: None,
        });

        let mut sealed_sector_b: StagedSectorMetadata = Default::default();

        sealed_sector_b.pieces.push(PieceMetadata {
            piece_key: String::from("x"),
            num_bytes: UnpaddedBytesAmount(128),
            comm_p: None,
        });

        let staged_sectors = vec![sealed_sector_a.clone(), sealed_sector_b.clone()];

        // piece takes up all remaining space in first sector
        match compute_destination_sector_id(
            &staged_sectors,
            UnpaddedBytesAmount(256),
            UnpaddedBytesAmount(128),
        ) {
            Ok(Some(destination_sector_id)) => {
                assert_eq!(destination_sector_id, sealed_sector_a.sector_id)
            }
            _ => panic!(),
        }

        // piece doesn't fit into the first, but does the second
        match compute_destination_sector_id(
            &staged_sectors,
            UnpaddedBytesAmount(256),
            UnpaddedBytesAmount(128),
        ) {
            Ok(Some(destination_sector_id)) => {
                assert_eq!(destination_sector_id, sealed_sector_b.sector_id)
            }
            _ => panic!(),
        }

        // piece doesn't fit into any in the list
        match compute_destination_sector_id(
            &staged_sectors,
            UnpaddedBytesAmount(256),
            UnpaddedBytesAmount(256),
        ) {
            Ok(None) => (),
            _ => panic!(),
        }

        // piece is over max
        match compute_destination_sector_id(
            &staged_sectors,
            UnpaddedBytesAmount(256),
            UnpaddedBytesAmount(257),
        ) {
            Err(_) => (),
            _ => panic!(),
        }
    }
}
