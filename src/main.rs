#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: elivagar <pbf> <out.pmtiles> [--tmp-dir path] [--ocean path.shp] [--skip-to ocean|sort] [--in-memory]"
        );
        std::process::exit(1);
    }

    let pbf_path = std::path::PathBuf::from(&args[1]);
    let output_path = std::path::PathBuf::from(&args[2]);

    let mut tmp_dir = std::path::PathBuf::from(".tilegen_tmp");
    let mut ocean_shapefile = None;
    let mut skip_to = None;
    let mut in_memory = false;

    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--tmp-dir" => {
                i += 1;
                tmp_dir = std::path::PathBuf::from(&args[i]);
            }
            "--ocean" => {
                i += 1;
                ocean_shapefile = Some(std::path::PathBuf::from(&args[i]));
            }
            "--skip-to" => {
                i += 1;
                skip_to = Some(args[i].clone());
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
        skip_to,
        in_memory,
    };

    elivagar::run(&config);
}
