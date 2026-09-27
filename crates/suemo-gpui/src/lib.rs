//! GPUI front-end for suemo (day view, week view, CRUD). M1 pins the fork
//! deps (MDRV.md §Registry) and hosts the `suemo` binary; the shell lands
//! in M2 (proposal §Milestones).

pub fn run() -> anyhow::Result<()> {
    // M2: full-window app (NOT an overlay), day view read-only +
    // socket-watch live updates.
    anyhow::bail!("suemo gui lands in M2 (proposal §Milestones)")
}
