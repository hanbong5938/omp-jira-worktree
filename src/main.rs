mod app;
mod checkouts;
mod config;
mod handoff;
mod jira;
mod launch;
mod model;
mod process;
mod state;
mod text;
mod transitions;
mod view;

fn main() {
    std::process::exit(app::main());
}
