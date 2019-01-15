use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::time;

use super::Backend;
use noria::{DataType, SyncTable};

const NANOS_PER_SEC: u64 = 1_000_000_000;
macro_rules! dur_to_fsec {
    ($d:expr) => {{
        let d = $d;
        (d.as_secs() * NANOS_PER_SEC + d.subsec_nanos() as u64) as f64 / NANOS_PER_SEC as f64
    }};
}

fn do_put<'a>(mutator: &'a mut SyncTable, tx: bool) -> Box<FnMut(Vec<DataType>) + 'a> {
    match tx {
        //true => Box::new(move |v| assert!(mutator.transactional.insert(v, Token::empty()).is_ok())),
        true => unimplemented!(),
        false => Box::new(move |v| assert!(mutator.insert(v).is_ok())),
    }
}

fn populate_table(backend: &mut Backend, data: &Path, use_txn: bool) -> usize {
    use std::str::FromStr;

    let table_name = data.file_stem().unwrap().to_str().unwrap();
    let mut putter = backend.g.table(table_name).unwrap().into_sync();

    let f = File::open(data).unwrap();
    let mut reader = BufReader::new(f);

    let mut s = String::new();
    println!("Populating {}...", table_name);
    let start = time::Instant::now();
    let mut i = 0;
    while reader.read_line(&mut s).unwrap() > 0 {
        {
            let fields: Vec<&str> = s.split("\t").map(str::trim).collect();
            let rec: Vec<DataType> = fields
                .into_iter()
                .map(|s| match i64::from_str(s) {
                    Ok(v) => v.into(),
                    Err(_) => s.into(),
                })
                .collect();
            do_put(&mut putter, use_txn)(rec);
        }
        i += 1;
        s.clear();
    }
    let dur = dur_to_fsec!(start.elapsed());
    println!(
        "Inserted {} {} records in {:.2}s ({:.2} PUTs/sec)!",
        i,
        table_name,
        dur,
        i as f64 / dur
    );
    i as usize
}

pub fn populate(backend: &mut Backend, data_location: &str, use_txn: bool) -> io::Result<()> {
    use std::fs;

    let dir = Path::new(data_location);
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                populate_table(backend, &path, use_txn);
            }
        }
    }
    Ok(())
}
