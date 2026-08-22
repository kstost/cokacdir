use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use crate::services::file_ops;
use crate::services::remote::{RemoteProfile, SftpSession};

#[derive(Debug, Clone)]
pub struct LocalDirectorySizeRequest {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RemoteDirectorySizeRequest {
    pub name: String,
    pub path: String,
}

#[derive(Debug)]
pub enum DirectorySizeMessage {
    Calculated {
        generation: u64,
        name: String,
        result: Result<u64, String>,
    },
    Finished {
        generation: u64,
        cancelled: bool,
    },
}

pub fn spawn_local(
    generation: u64,
    requests: Vec<LocalDirectorySizeRequest>,
    cancel_flag: Arc<AtomicBool>,
) -> Receiver<DirectorySizeMessage> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut cancelled = is_cancelled(&cancel_flag);
        for request in requests {
            if is_cancelled(&cancel_flag) {
                cancelled = true;
                break;
            }

            let result = match file_ops::calculate_total_size(&[request.path], &cancel_flag) {
                Ok((bytes, _)) => Ok(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    cancelled = true;
                    break;
                }
                Err(error) => Err(error.to_string()),
            };
            if is_cancelled(&cancel_flag) {
                cancelled = true;
                break;
            }

            if sender
                .send(DirectorySizeMessage::Calculated {
                    generation,
                    name: request.name,
                    result,
                })
                .is_err()
            {
                return;
            }
        }

        let _ = sender.send(DirectorySizeMessage::Finished {
            generation,
            cancelled,
        });
    });
    receiver
}

pub fn spawn_remote(
    generation: u64,
    profile: RemoteProfile,
    requests: Vec<RemoteDirectorySizeRequest>,
    cancel_flag: Arc<AtomicBool>,
) -> Receiver<DirectorySizeMessage> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut cancelled = is_cancelled(&cancel_flag);
        if !cancelled {
            match SftpSession::connect(&profile) {
                Ok(session) => {
                    for request in requests {
                        if is_cancelled(&cancel_flag) {
                            cancelled = true;
                            break;
                        }
                        let result =
                            calculate_remote_directory_size(&session, &request.path, &cancel_flag);
                        if is_cancelled(&cancel_flag)
                            || matches!(&result, Err(error) if error == "Cancelled")
                        {
                            cancelled = true;
                            break;
                        }
                        if send_calculated(&sender, generation, request.name, result).is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    if is_cancelled(&cancel_flag) {
                        cancelled = true;
                    } else {
                        for request in requests {
                            if send_calculated(
                                &sender,
                                generation,
                                request.name,
                                Err(error.clone()),
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
        }

        let _ = sender.send(DirectorySizeMessage::Finished {
            generation,
            cancelled,
        });
    });
    receiver
}

fn send_calculated(
    sender: &Sender<DirectorySizeMessage>,
    generation: u64,
    name: String,
    result: Result<u64, String>,
) -> Result<(), mpsc::SendError<DirectorySizeMessage>> {
    sender.send(DirectorySizeMessage::Calculated {
        generation,
        name,
        result,
    })
}

fn is_cancelled(cancel_flag: &Arc<AtomicBool>) -> bool {
    cancel_flag.load(Ordering::Relaxed)
}

fn calculate_remote_directory_size(
    session: &SftpSession,
    root: &str,
    cancel_flag: &Arc<AtomicBool>,
) -> Result<u64, String> {
    let mut total = 0u64;
    let mut pending = vec![root.to_string()];

    while let Some(path) = pending.pop() {
        if is_cancelled(cancel_flag) {
            return Err("Cancelled".to_string());
        }
        let entries = session.list_dir(&path)?;
        if is_cancelled(cancel_flag) {
            return Err("Cancelled".to_string());
        }

        for entry in entries {
            if is_cancelled(cancel_flag) {
                return Err("Cancelled".to_string());
            }
            if entry.is_symlink {
                continue;
            }
            if entry.is_directory {
                pending.push(remote_child_path(&path, &entry.name)?);
            } else {
                total = total.saturating_add(entry.size);
            }
        }
    }

    Ok(total)
}

fn remote_child_path(parent: &str, name: &str) -> Result<String, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err("Remote entry has an unsafe or ambiguous name".to_string());
    }
    if parent == "/" {
        Ok(format!("/{name}"))
    } else {
        Ok(format!("{}/{name}", parent.trim_end_matches('/')))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn receive_until_finished(
        receiver: Receiver<DirectorySizeMessage>,
    ) -> Vec<DirectorySizeMessage> {
        let mut messages = Vec::new();
        loop {
            let message = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            let finished = matches!(message, DirectorySizeMessage::Finished { .. });
            messages.push(message);
            if finished {
                return messages;
            }
        }
    }

    #[test]
    fn local_worker_reports_recursive_content_size() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("folder");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("one.bin"), vec![0u8; 7]).unwrap();
        fs::write(root.join("nested/two.bin"), vec![0u8; 11]).unwrap();

        let receiver = spawn_local(
            4,
            vec![LocalDirectorySizeRequest {
                name: "folder".to_string(),
                path: root,
            }],
            Arc::new(AtomicBool::new(false)),
        );
        let messages = receive_until_finished(receiver);

        assert!(matches!(
            &messages[0],
            DirectorySizeMessage::Calculated {
                generation: 4,
                name,
                result: Ok(18),
            } if name == "folder"
        ));
        assert!(matches!(
            &messages[1],
            DirectorySizeMessage::Finished {
                generation: 4,
                cancelled: false,
            }
        ));
    }

    #[test]
    fn local_worker_honors_preexisting_cancellation() {
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let receiver = spawn_local(9, Vec::new(), cancel_flag);
        let messages = receive_until_finished(receiver);

        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            DirectorySizeMessage::Finished {
                generation: 9,
                cancelled: true,
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn local_worker_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("folder");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("inside.bin"), vec![0u8; 3]).unwrap();
        fs::write(outside.join("outside.bin"), vec![0u8; 101]).unwrap();
        symlink(&outside, root.join("link")).unwrap();

        let receiver = spawn_local(
            12,
            vec![LocalDirectorySizeRequest {
                name: "folder".to_string(),
                path: root,
            }],
            Arc::new(AtomicBool::new(false)),
        );
        let messages = receive_until_finished(receiver);

        assert!(matches!(
            &messages[0],
            DirectorySizeMessage::Calculated { result: Ok(3), .. }
        ));
    }

    #[test]
    fn remote_child_paths_reject_ambiguous_names() {
        assert_eq!(remote_child_path("/base", "ok").unwrap(), "/base/ok");
        assert!(remote_child_path("/base", "../escape").is_err());
        assert!(remote_child_path("/base", "bad/name").is_err());
        assert!(remote_child_path("/base", "bad\0name").is_err());
    }
}
