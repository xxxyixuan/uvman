#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, visible_alias = "ls")]
pub struct List {
    pub tool: Option<String>,

    #[clap(short = 'r', long)]
    pub remote: bool,

    #[clap(long)]
    pub limit: usize,

    #[clap(short = 'J', long)]
    pub json: bool,
}

impl List {
    pub async fn run(&self) -> crate::Result<()> {
        if !self.remote {}
        Ok(())
    }
}
