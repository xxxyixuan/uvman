use crate::Result;

/// test info
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, hide = true)]
pub struct Info {
    pub str: String,
}

impl Info {
    pub fn run(&self) -> Result<()> {
        println!("Info: {}", self.str);
        Ok(())
    }
}
