use ec2rust::process_args;
use ec2rust::Args;
use clap::Parser;

fn main() {
    let args = Args::parse();
    let (tcfg, cc_json_path, extra_args) = process_args(args);
    let extra_args = extra_args.iter().map(AsRef::as_ref).collect::<Vec<_>>();
    c2rust_transpile::export(tcfg, &cc_json_path, &extra_args);
}
