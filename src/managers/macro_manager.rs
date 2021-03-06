use std::{
    fs,
    io::{self, BufRead, Write},
    path::PathBuf,
    process::ChildStdin,
    sync::{mpsc, Arc, Mutex},
    thread,
};

use regex::Regex;
use rlua::{Lua, MultiValue};

use crate::event_processor::EventProcessor;

use super::server_instance::Status;
use log::{error, info};

#[derive(Clone)]
pub struct MacroManager {
    pub path_to_macros: PathBuf,
    pub path_to_instance: PathBuf,
    stdin_sender: Arc<Mutex<Option<ChildStdin>>>,
    event_processor: Arc<Mutex<EventProcessor>>,
    players_online: Arc<Mutex<Vec<String>>>,
    status: Arc<Mutex<Status>>,
}

impl MacroManager {
    pub fn new(
        path_to_macros: PathBuf,
        path_to_instance: PathBuf,
        stdin_sender: Arc<Mutex<Option<ChildStdin>>>,
        event_processor: Arc<Mutex<EventProcessor>>,
        players_online: Arc<Mutex<Vec<String>>>,
        status: Arc<Mutex<Status>>,
    ) -> MacroManager {
        fs::create_dir_all(path_to_macros.as_path()).map_err(|e| e.to_string());
        MacroManager {
            path_to_macros,
            path_to_instance,
            stdin_sender,
            event_processor,
            players_online,
            status,
        }
    }
    pub fn run(
        &self,
        name: String,
        args: Vec<String>,
        executor: Option<String>,
    ) -> Result<(), String> {
        let macro_file = fs::File::open(self.path_to_macros.join(name.clone()).with_extension("lua"))
            .map_err(|e| e.to_string())?;
        let mut program: String = String::new();

        for line_result in io::BufReader::new(macro_file).lines() {
            program.push_str(format!("{}\n", line_result.unwrap()).as_str());
        }

        Lua::new().context(move |lua_ctx| {
            for (pos, arg) in args.iter().enumerate() {
                lua_ctx
                    .globals()
                    .set(format!("arg{}", pos + 1), arg.clone());
            }
            let delay_sec = lua_ctx
                .create_function(|_, time: u64| {
                    thread::sleep(std::time::Duration::from_secs(time));
                    Ok(())
                })
                .unwrap();
            lua_ctx.globals().set("delay_sec", delay_sec);
            lua_ctx.globals().set("EXECUTOR", executor);
            lua_ctx
                .globals()
                .set("PATH_TO_INSTANCE", self.path_to_instance.to_str());

            let event_processor = self.event_processor.clone();
            let await_msg = lua_ctx
                .create_function(move |_lua_ctx, ()| {
                    let (tx, rx) = mpsc::channel();
                    let tx = Arc::new(Mutex::new(tx));
                    let index = event_processor.lock().unwrap().on_chat.len();
                    event_processor.lock().unwrap().on_chat.push(Arc::new(
                        move |player, player_msg| {
                            tx.lock().unwrap().send((player, player_msg)).unwrap();
                        },
                    ));
                    let (player, player_msg) = rx.recv().unwrap();
                    // remove the callback
                    event_processor.lock().unwrap().on_chat.remove(index);
                    Ok((player, player_msg))
                })
                .unwrap();
            lua_ctx.globals().set("await_msg", await_msg);
            let delay_milli = lua_ctx
                .create_function(|_, time: u64| {
                    thread::sleep(std::time::Duration::from_millis(time));
                    Ok(())
                })
                .unwrap();
            lua_ctx.globals().set("delay_milli", delay_milli);
            let stdin_sender_closure = self.stdin_sender.clone();
            let send_stdin = lua_ctx
                .create_function(move |ctx, line: String| {
                    let reg = Regex::new(r"\$\{(\w*)\}").unwrap();
                    let globals = ctx.globals();
                    let mut after = line.clone();
                    if reg.is_match(&line) {
                        for cap in reg.captures_iter(&line) {
                            after = after.replace(
                                format!("${{{}}}", &cap[1]).as_str(),
                                &globals.get::<_, String>(cap[1].to_string()).unwrap(),
                            );
                        }

                        stdin_sender_closure
                            .lock()
                            .unwrap()
                            .as_mut()
                            .unwrap()
                            .write_all(format!("{}\n", after).as_bytes());
                    } else {
                        stdin_sender_closure
                            .lock()
                            .unwrap()
                            .as_mut()
                            .unwrap()
                            .write_all(format!("{}\n", line).as_bytes());
                    }
                    Ok(())
                })
                .unwrap();
            lua_ctx.globals().set("send_stdin", send_stdin);
            let players_online = self.players_online.clone();
            lua_ctx.globals().set(
                "get_players_online",
                lua_ctx
                    .create_function(move |_, ()| {
                        let mut players_online_vec = Vec::new();
                        for player in players_online.lock().unwrap().iter() {
                            players_online_vec.push(player.clone());
                        }
                        Ok(players_online_vec)
                    })
                    .unwrap(),
            );
            let status = self.status.clone();
            lua_ctx.globals().set(
                "get_status",
                lua_ctx
                    .create_function(move |_, ()| Ok(status.lock().unwrap().to_string()))
                    .unwrap(),
            );

            lua_ctx.globals().set(
                "isBadWord",
                lua_ctx
                    .create_function(|_, word: String| {
                        use censor::*;
                        let censor = Standard + "lambda";
                        Ok((censor.check(word.as_str()),))
                    })
                    .unwrap(),
            );
            let stdin_sender_closure = self.stdin_sender.clone();
            let instance_name = self.path_to_instance.file_name().unwrap().to_str().unwrap().to_string();
            let macro_name = name.clone();
            lua_ctx.globals().set(
                "log_info",
                lua_ctx
                    .create_function(move |_, msg: String| {
                        stdin_sender_closure
                            .lock()
                            .unwrap()
                            .as_mut()
                            .unwrap()
                            .write_all(
                                format!(
                                    "tellraw @a [\"\",{{\"text\":\"[Info] \",\"color\":\"green\"}},{{\"text\":\"{}\"}}]\n",
                                    msg
                                )
                                .as_bytes(),
                            );
                        info!("[{}] [MacroManager:{}] {}", instance_name, macro_name, msg );
                        Ok(())
                    })
                    .unwrap(),
            );
            let stdin_sender_closure = self.stdin_sender.clone();
            let instance_name = self.path_to_instance.file_name().unwrap().to_str().unwrap().to_string();
            let macro_name = name.clone();

            lua_ctx.globals().set(
                "log_warn",
                lua_ctx
                    .create_function(move |_, msg: String| {
                        stdin_sender_closure
                            .lock()
                            .unwrap()
                            .as_mut()
                            .unwrap()
                            .write_all(
                                format!(
                                    "tellraw @a [\"\",{{\"text\":\"[Warn] \",\"color\":\"yellow\"}},{{\"text\":\"{}\"}}]\n",
                                    msg
                                )
                                .as_bytes(),
                            );
                        warn!("[{}] [MacroManager:{}] {}", instance_name, macro_name, msg );
                        Ok(())
                    })
                    .unwrap(),
            );
            let stdin_sender_closure = self.stdin_sender.clone();
            let instance_name = self.path_to_instance.file_name().unwrap().to_str().unwrap().to_string();
            let macro_name = name.clone();

            lua_ctx.globals().set(
                "log_err",
                lua_ctx
                    .create_function(move |_, msg: String| {
                        stdin_sender_closure
                            .lock()
                            .unwrap()
                            .as_mut()
                            .unwrap()
                            .write_all(
                                format!(
                                    "tellraw @a [\"\",{{\"text\":\"[Error] \",\"color\":\"red\"}},{{\"text\":\"{}\"}}]\n",
                                    msg
                                )
                                .as_bytes(),
                            );
                        error!("[{}] [MacroManager:{}] {}", instance_name, macro_name, msg );
                        Ok(())
                    })
                    .unwrap(),
            );
            let instance_name = self.path_to_instance.file_name().unwrap().to_str().unwrap().to_string();
            let macro_name = name.clone();
            match lua_ctx.load(&program).eval::<MultiValue>() {
                Ok(value) => {
                    let string_value = value
                    .iter()
                    .map(|value| format!("{:?}", value))
                    .collect::<Vec<_>>().join("\t");
                    if !string_value.is_empty() {
                        info!(
                            "{}",
                            string_value
                        );
                    }
                }
                Err(e) => {
                    error!("[{}] [MacroManager:{}] {}",instance_name, macro_name, e);
                }
            }
        });
        Ok(())
    }

    /// Set the macro manager's stdin sender.
    pub fn set_stdin_sender(&mut self, stdin_sender: Arc<Mutex<Option<ChildStdin>>>) {
        self.stdin_sender = stdin_sender;
    }

    /// Set the macro manager's event processor.
    pub fn set_event_processor(&mut self, event_processor: Arc<Mutex<EventProcessor>>) {
        self.event_processor = event_processor;
    }
}
