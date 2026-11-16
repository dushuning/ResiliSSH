//! 命令行版本的入口；全部逻辑在库里的 `cli` 模块，与图形界面共用同一套传输引擎。

fn main() {
    std::process::exit(sshutil_lib::cli::run(std::env::args().skip(1).collect()));
}
