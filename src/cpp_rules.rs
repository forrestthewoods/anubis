#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]

use serde::Deserialize;
use std::path::PathBuf;


#[derive(Clone, Debug, Deserialize)]
pub struct CppBinary {
    pub name: String,
    pub srcs: Vec<String>,
    pub srcs2: Vec<PathBuf>,
    pub srcs3: Vec<String>,
    pub srcs4: Vec<String>,
}
