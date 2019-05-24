use super::{Command, CommandGroup, Configuration};
use crate::client::Context;
use crate::model::channel::Message;
use uwl::{StrExt, StringStream};

#[derive(Debug, Clone, PartialEq)]
pub enum Prefix<'a> {
    Punct(&'a str),
    Mention(&'a str),
    None,
    #[doc(hidden)]
    __Nonexhaustive,
}

pub fn parse_prefix<'a>(
    ctx: &mut Context,
    msg: &'a Message,
    config: &Configuration,
) -> (Prefix<'a>, &'a str) {
    let mut stream = StringStream::new(&msg.content);
    stream.take_while(|s| s.is_whitespace());

    if let Some(ref mention) = config.on_mention {
        if let Ok(id) = stream.parse("<@(!){}>") {
            if id.is_numeric() && mention == id {
                stream.take_while(|s| s.is_whitespace());
                return (Prefix::Mention(id), stream.rest());
            }
        }
    }

    let mut prefix = None;
    if !config.prefixes.is_empty() || !config.dynamic_prefixes.is_empty() {
        for f in &config.dynamic_prefixes {
            if let Some(p) = f(ctx, msg) {
                let pp = stream.peek_for(p.chars().count());

                if p == pp {
                    prefix = Some(pp);
                    break;
                }
            }
        }

        for p in &config.prefixes {
            // If `dynamic_prefixes` succeeded, don't iterate through the normal prefixes.
            if prefix.is_some() {
                break;
            }

            let pp = stream.peek_for(p.chars().count());

            if p == pp {
                prefix = Some(pp);
                break;
            }
        }
    }

    if let Some(prefix) = prefix {
        stream.increment(prefix.len());

        if config.with_whitespace.prefixes {
            stream.take_while(|s| s.is_whitespace());
        }

        let args = stream.rest();

        return (Prefix::Punct(prefix), args.trim());
    }

    if config.with_whitespace.prefixes {
        stream.take_while(|s| s.is_whitespace());
    }

    let args = stream.rest();
    (Prefix::None, args.trim())
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ParseMode {
    BySpace,
    ByLength,
}

struct CommandParser<'msg, 'groups, 'config> {
    stream: StringStream<'msg>,
    groups: &'groups [&'static CommandGroup],
    config: &'config Configuration,
    mode: ParseMode,
    unrecognised: Option<&'msg str>,
}

impl<'msg, 'groups, 'config> CommandParser<'msg, 'groups, 'config> {
    fn new(stream: StringStream<'msg>, groups: &'groups [&'static CommandGroup], config: &'config Configuration) -> Self {
        CommandParser {
            stream,
            groups,
            config,
            mode: if config.by_space { ParseMode::BySpace } else { ParseMode::ByLength },
            unrecognised: None,
        }
    }

    fn next_text(&self, length: impl FnOnce() -> usize) -> &'msg str {
        match self.mode {
            ParseMode::ByLength => self.stream.peek_for(length()),
            ParseMode::BySpace => self.stream.peek_until(|s| s.is_whitespace()),
        }
    }

    fn as_lowercase(&self, s: &str, f: impl FnOnce(&str) -> bool) -> bool {
        if self.config.case_insensitive {
            let s = s.to_lowercase();
            f(&s)
        } else {
            f(s)
        }
    }

    fn command(&mut self, command: &'static Command) -> Option<&'static Command> {
        for name in command.options.names {
            // FIXME: If `by_space` option is set true, we shouldn't be retrieving the block of text
            // again and again for the command name.
            let n = self.next_text(|| name.chars().count());

            let equals = self.as_lowercase(n, |n| n == *name && !self.config.disabled_commands.contains(n));

            if equals {
                self.stream.increment(n.len());

                if self.config.with_whitespace.commands {
                    self.stream.take_while(|s| s.is_whitespace());
                }

                for sub in command.options.sub_commands {
                    if let Some(cmd) = self.command(sub) {
                        self.unrecognised = None;
                        return Some(cmd);
                    }
                }

                self.unrecognised = None;
                return Some(command);
            }

            self.unrecognised = Some(n);
        }

        None
    }

    fn group(&mut self, group: &'static CommandGroup) -> (Option<&'msg str>, &'static CommandGroup) {
        for p in group.options.prefixes {
            let pp = self.next_text(|| p.chars().count());

            if *p == pp {
                self.stream.increment(pp.len());

                if self.config.with_whitespace.groups {
                    self.stream.take_while(|s| s.is_whitespace());
                }

                for sub_group in group.sub_groups {
                    let x = self.group(*sub_group);

                    if x.0.is_some() {
                        return x;
                    }
                }

                return (Some(pp), group);
            }
        }

        (None, group)
    }

    fn parse(mut self, prefix: Prefix<'msg>) -> Result<Invoke<'msg>, Option<&'msg str>> {
        let pos = self.stream.offset();
        for group in self.groups {
            let (gprefix, group) = self.group(*group);

            if gprefix.is_none() && !group.options.prefixes.is_empty() {
                unsafe { self.stream.set_unchecked(pos) };
                continue;
            }

            for command in group.commands {
                if let Some(command) = self.command(command) {
                    return Ok(Invoke::Command {
                        prefix,
                        group,
                        gprefix,
                        command,
                        args: self.stream.rest(),
                    });
                }
            }

            // Only execute the default command if a group prefix is present.
            if let Some(command) = group.options.default_command {
                if gprefix.is_some() {
                    return Ok(Invoke::Command {
                        prefix,
                        group,
                        gprefix,
                        command,
                        args: self.stream.rest(),
                    });
                }
            }

            unsafe { self.stream.set_unchecked(pos) };
        }

        Err(self.unrecognised)
    }
}

pub(crate) fn parse_command<'a>(
    msg: &'a str,
    prefix: Prefix<'a>,
    groups: &[&'static CommandGroup],
    config: &Configuration,
    help_was_set: Option<&[&'static str]>,
) -> Result<Invoke<'a>, Option<&'a str>> {
    let mut stream = StringStream::new(msg);
    stream.take_while(|s| s.is_whitespace());

    // We take precedence over commands named help command's name.
    if let Some(names) = help_was_set {
        for name in names {
            if stream.eat(name) {
                stream.take_while(|s| s.is_whitespace());

                let args = stream.rest();

                return Ok(Invoke::Help { prefix, name, args });
            }
        }
    }

    CommandParser::new(stream, groups, config).parse(prefix)
}

#[derive(Debug)]
pub enum Invoke<'a> {
    Command {
        prefix: Prefix<'a>,
        // Group prefix
        gprefix: Option<&'a str>,

        group: &'static CommandGroup,
        command: &'static Command,
        args: &'a str,
    },
    Help {
        prefix: Prefix<'a>,
        name: &'static str,
        args: &'a str,
    },
}