//! Run settings: where the run lives and the info API's budget. The strategies' own settings live in their modules.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    /// Info API weight per minute this process may use (1200 per IP in total).
    pub weight_per_min: f64,
    /// Postgres to keep the run in (env `DATABASE_URL`); files in `data_dir` without it.
    pub database_url: Option<String>,
    /// Name of this run in the database (env `RUN_ID`, `--run`): runs share its tables.
    pub run_id: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            weight_per_min: 400.0,
            database_url: std::env::var("DATABASE_URL").ok().filter(|s| !s.trim().is_empty()),
            run_id: std::env::var("RUN_ID").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "main".into()),
        }
    }
}

impl Config {
    /// `--data DIR`, `--weight N`, `--run NAME`; any other option (with its value) is left out and returned (the copy
    /// trading's `--start`, `--risk`, ... may still sit in a deploy's `EXTRA_ARGS`: not an error).
    pub fn from_args(args: &[String]) -> anyhow::Result<(Self, Vec<String>)> {
        let mut c = Self::default();
        let mut rest = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let v = || args.get(i + 1).cloned().ok_or_else(|| anyhow::anyhow!("{} needs a value", args[i]));
            match args[i].as_str() {
                "--data" => c.data_dir = PathBuf::from(v()?),
                "--weight" => c.weight_per_min = v()?.parse()?,
                "--run" => c.run_id = v()?,
                _ => {
                    rest.push(args[i].clone());
                    if let Some(x) = args.get(i + 1) {
                        rest.push(x.clone());
                    }
                }
            }
            i += 2;
        }
        Ok((c, rest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn the_run_options_and_the_rest_left_out() {
        let (c, rest) = Config::from_args(&args("--data /d --weight 800 --run x --start 2000")).unwrap();
        assert_eq!((c.data_dir, c.weight_per_min, c.run_id.as_str()), (PathBuf::from("/d"), 800.0, "x"));
        assert_eq!(rest, args("--start 2000"));
        let (d, rest) = Config::from_args(&[]).unwrap();
        assert!(rest.is_empty() && d.weight_per_min == 400.0 && d.data_dir == PathBuf::from("data"));
        assert!(Config::from_args(&args("--weight")).is_err());
        assert!(Config::from_args(&args("--weight lots")).is_err());
    }
}
