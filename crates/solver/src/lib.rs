//! Inverse lineup search: stand spots, coarse voxel sweep, refinement, exact
//! verification under aim perturbation, stability scoring and ranking.
//!
//! Ported from `cs2-smoke-solver/src/Solver/`; see each module's doc comment
//! for exact file:line citations. Part A owns `standspots`/`origins`/
//! `nav_ground`; part B (this crate's other modules) owns the solver core:
//! reach tables, free-space pruning, landing zones, the coarse sweep, exact
//! verification, lineup identity, human-error estimation and aim reference;
//! part C owns the orchestration and ranking (`target`, `rank`).

pub mod aim_reference;
pub(crate) mod dotnet_sort;
pub mod free_space;
pub mod human_error;
pub mod identity;
pub mod lineup;
pub mod nav_ground;
pub mod origins;
pub mod rank;
pub mod reach;
pub mod standspots;
pub mod sweep;
pub mod target;
pub mod verify;
pub mod zone;
