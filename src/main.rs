#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: elivagar <pbf> <out.pmtiles> [--tmp-dir path] [--ocean path.shp] [--ocean-simplified path.shp] [--skip-to ocean|sort] [--in-memory]"
        );
        std::process::exit(1);
    }

    let pbf_path = std::path::PathBuf::from(&args[1]);
    let output_path = std::path::PathBuf::from(&args[2]);

    let mut tmp_dir = std::path::PathBuf::from(".tilegen_tmp");
    let mut ocean_shapefile = None;
    let mut ocean_simplified_shapefile = None;
    let mut skip_to: Option<elivagar::SkipTo> = None;
    let mut in_memory = false;

    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--tmp-dir" | "--ocean" | "--ocean-simplified" | "--skip-to" => {
                let flag = &args[i];
                i += 1;
                if i >= args.len() {
                    eprintln!("{flag} requires a value");
                    std::process::exit(1);
                }
                match flag.as_str() {
                    "--tmp-dir" => tmp_dir = std::path::PathBuf::from(&args[i]),
                    "--ocean" => ocean_shapefile = Some(std::path::PathBuf::from(&args[i])),
                    "--ocean-simplified" => ocean_simplified_shapefile = Some(std::path::PathBuf::from(&args[i])),
                    "--skip-to" => {
                        skip_to = Some(match args[i].as_str() {
                            "ocean" => elivagar::SkipTo::Ocean,
                            "sort" => elivagar::SkipTo::Sort,
                            other => {
                                eprintln!("Unknown skip-to value: {other} (expected ocean or sort)");
                                std::process::exit(1);
                            }
                        });
                    }
                    _ => unreachable!(),
                }
            }
            "--in-memory" => {
                in_memory = true;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let config = elivagar::TilegenConfig {
        pbf_path,
        output_path,
        tmp_dir,
        min_zoom: 0,
        max_zoom: 14,
        ocean_shapefile,
        ocean_simplified_shapefile,
        skip_to,
        in_memory,
    };

    if let Err(e) = elivagar::run(&config) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
