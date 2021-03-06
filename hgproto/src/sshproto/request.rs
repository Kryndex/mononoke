// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::collections::HashMap;
use std::iter;
use std::str::{self, FromStr};

use bytes::BytesMut;

use nom::{ErrorKind, FindSubstring, IResult, Needed, Slice, is_digit};

use mercurial_types::NodeHash;

use batch;
use errors;
use errors::*;
use {GetbundleArgs, Request};


/// Parse an unsigned decimal integer. If it reaches the end of input, it returns Incomplete,
/// as there may be more digits following
fn digit<F: Fn(u8) -> bool>(input: &[u8], isdigit: F) -> IResult<&[u8], &[u8]> {
    for (idx, item) in input.iter().enumerate() {
        if !isdigit(*item) {
            if idx == 0 {
                return IResult::Error(ErrorKind::Digit);
            } else {
                return IResult::Done(&input[idx..], &input[0..idx]);
            }
        }
    }
    IResult::Incomplete(Needed::Unknown)
}

named!(integer<usize>,
       map_res!(map_res!(apply!(digit, is_digit), str::from_utf8), FromStr::from_str));

/// Return an identifier of the form [a-zA-Z_][a-zA-Z0-9_]*. Returns Incomplete
/// if it manages to reach the end of input, as there may be more identifier coming.
fn ident(input: &[u8]) -> IResult<&[u8], &[u8]> {
    for (idx, item) in input.iter().enumerate() {
        match *item as char {
            'a'...'z' | 'A'...'Z' | '_' => continue,
            '0'...'9' if idx > 0 => continue,
            _ => {
                if idx > 0 {
                    return IResult::Done(&input[idx..], &input[0..idx]);
                } else {
                    return IResult::Error(ErrorKind::AlphaNumeric);
                }
            }
        }
    }
    IResult::Incomplete(Needed::Unknown)
}

/// As above, but assumes input is complete, so reaching the end of input means
/// the identifier is the entire input.
fn ident_complete(input: &[u8]) -> IResult<&[u8], &[u8]> {
    match ident(input) {
        IResult::Incomplete(_) => IResult::Done(b"", input),
        other => other,
    }
}

/// A "*" parameter is a meta-parameter - its argument is a count of
/// a number of other parameters. (We accept nested/recursive star parameters,
/// but I don't know if that ever happens in practice.)
named!(param_star<HashMap<Vec<u8>, Vec<u8>>>,
    do_parse!(
        tag!(b"* ") >>
        count: integer >> tag!(b"\n") >>
        res: apply!(params, count) >>
        (res)
    )
);

/// A named parameter is a name followed by a decimal integer of the number of
/// bytes in the parameter, followed by newline. The parameter value has no terminator.
/// ident <bytelen>\n
/// <bytelen bytes>
named!(param_kv<HashMap<Vec<u8>, Vec<u8>>>,
    do_parse!(
        key: ident >> tag!(b" ") >>
        len: integer >> tag!(b"\n") >>
        val: take!(len) >>
        (iter::once((key.to_vec(), val.to_vec())).collect())
    )
);

/// Normal ssh protocol params:
/// either a "*", which indicates a number of following parameters,
/// or a named parameter whose value bytes follow.
/// "count" is the number of required parameters, including the "*" parameter - but *not*
/// the parameters that the "*" parameter expands to.
fn params(inp: &[u8], count: usize) -> IResult<&[u8], HashMap<Vec<u8>, Vec<u8>>> {
    let mut inp = inp;
    let mut have = 0;

    let mut ret = HashMap::with_capacity(count);

    while have < count {
        let res = alt!(inp,
              param_star => { |kv: HashMap<_, _>| { have += 1; kv } }
            | param_kv => { |kv: HashMap<_, _>| { have += kv.len(); kv } }
        );

        match res {
            IResult::Done(rest, val) => {
                for (k, v) in val.into_iter() {
                    ret.insert(k, v);
                }
                inp = rest;
            }
            failed => return failed,
        }
    }

    IResult::Done(inp, ret)
}

fn notcomma(b: u8) -> bool {
    b != b','
}

/// A batch parameter is "name=value", where name ad value are escaped with an ad-hoc
/// scheme to protect ',', ';', '=', ':'. The value ends either at the end of the input
/// (which is actually from the "batch" command "cmds" parameter), or at a ',', as they're
/// comma-delimited.
named!(batch_param_escaped<(Vec<u8>, Vec<u8>)>,
    map_res!(
        do_parse!(
            key: take_until_and_consume1!("=") >>
            val: take_while!(notcomma) >>
            ((key, val))
        ),
        |(k, v)| Ok::<_, Error>((batch::unescape(k)?, batch::unescape(v)?))
    )
);

/// Extract parameters from batch - same signature as params
/// Batch parameters are a comma-delimited list of parameters; count is unused
/// and there's no notion of star params.
named_args!(batch_params(_count: usize)<HashMap<Vec<u8>, Vec<u8>>>,
    map!(
        separated_list!(complete!(tag!(",")), complete!(batch_param_escaped)),
        |v: Vec<_>| v.into_iter().collect()
    )
);

/// A nodehash is simply 40 hex digits.
named!(nodehash<NodeHash>,
    map_res!(
        take!(40),
        |v: &[u8]| str::parse(str::from_utf8(v)?)
    )
);

/// A pair of nodehashes, separated by '-'
named!(pair<(NodeHash, NodeHash)>,
    do_parse!(a: nodehash >> tag!("-") >> b: nodehash >> ((a, b)))
);

/// A space-separated list of pairs.
named!(pairlist<Vec<(NodeHash, NodeHash)>>,
    separated_list!(complete!(tag!(" ")), pair)
);

/// A space-separated list of node hashes
named!(hashlist<Vec<NodeHash>>,
    separated_list!(complete!(tag!(" ")), nodehash)
);

/// A comma-separated list of arbitrary values. The input is assumed to be
/// complete and exact.
fn commavalues(input: &[u8]) -> IResult<&[u8], Vec<Vec<u8>>> {
    if input.len() == 0 {
        // Need to handle this separately because the below will return
        // vec![vec![]] on an empty input.
        IResult::Done(b"", vec![])
    } else {
        IResult::Done(
            b"",
            input
                .split(|c| *c == b',')
                .map(|val| val.to_vec())
                .collect(),
        )
    }
}

fn notsemi(b: u8) -> bool {
    b != b';'
}

/// A command in a batch. Commands are represented as "command parameters". The parameters
/// end either at the end of the buffer or at ';'.
named!(cmd<(Vec<u8>, Vec<u8>)>,
    do_parse!(
        cmd: take_until_and_consume1!(" ") >>
        args: take_while!(notsemi) >>
        ((cmd.to_vec(), args.to_vec()))
    )
);

/// A list of batched commands - the list is delimited by ';'.
named!(cmdlist<Vec<(Vec<u8>, Vec<u8>)>>,
    separated_list!(complete!(tag!(";")), cmd)
);

named!(match_eof<&'a [u8]>,
       eof!()
);
/// Given a hash of parameters, look up a parameter by name, and if it exists,
/// apply a parser to its value. If it doesn't, error out.
fn parseval<'a, F, T>(params: &'a HashMap<Vec<u8>, Vec<u8>>, key: &str, parser: F) -> Result<T>
where
    F: Fn(&'a [u8]) -> IResult<&'a [u8], T>,
{
    match params.get(key.as_bytes()) {
        None => bail!("missing param {}", key),
        Some(v) => {
            match parser(v.as_ref()) {
                IResult::Done(rest, v) => match match_eof(rest) {
                    IResult::Done(_, _) => Ok(v),
                    _ => bail!("Unconsumed characters remain after parsing param"),
                },
                IResult::Incomplete(err) => bail!("param parse incomplete: {:?}", err),
                IResult::Error(err) => bail!("param parse failed: {:?}", err),
            }
        }
    }
}

/// Given a hash of parameters, look up a parameter by name, and if it exists,
/// apply a parser to its value. If it doesn't, return the default value.
fn parseval_default<'a, F, T>(
    params: &'a HashMap<Vec<u8>, Vec<u8>>,
    key: &str,
    parser: F,
) -> Result<T>
where
    F: Fn(&'a [u8]) -> IResult<&'a [u8], T>,
    T: Default,
{
    match params.get(key.as_bytes()) {
        None => Ok(T::default()),
        Some(v) => {
            match parser(v.as_ref()) {
                IResult::Done(unparsed, v) => match match_eof(unparsed) {
                    IResult::Done(_, _) => Ok(v),
                    _ => bail!("Unconsumed characters remain after parsing param: {:?}", unparsed)
                },
                IResult::Incomplete(err) => bail!("param parse incomplete: {:?}", err),
                IResult::Error(err) => bail!("param parse failed: {:?}", err),
            }
        }
    }
}

/// Parse a command, given some input, a command name (used as a tag), a param parser
/// function (which generalizes over batched and non-batched parameter syntaxes),
/// number of args (since each command has a fixed number of expected parameters,
/// not withstanding '*'), and a function to actually produce a parsed `Request`.
fn parse_command<'a, C, F, T>(
    inp: &'a [u8],
    cmd: C,
    parse_params: fn(&[u8], usize)
        -> IResult<&[u8], HashMap<Vec<u8>, Vec<u8>>>,
    nargs: usize,
    func: F,
) -> IResult<&'a [u8], T>
where
    F: Fn(HashMap<Vec<u8>, Vec<u8>>) -> Result<T>,
    C: AsRef<[u8]>,
{
    let cmd = cmd.as_ref();
    let res = do_parse!(inp,
        tag!(cmd) >> tag!("\n") >>
        p: call!(parse_params, nargs) >> (p));

    match res {
        IResult::Done(rest, v) => {
            match func(v) {
                Ok(t) => IResult::Done(rest, t),
                Err(_e) => IResult::Error(ErrorKind::Custom(999999)),    // ugh
            }
        }
        IResult::Error(e) => IResult::Error(e),
        IResult::Incomplete(n) => IResult::Incomplete(n),
    }
}

/// Parse an ident, and map it to `String`.
fn ident_string(inp: &[u8]) -> IResult<&[u8], String> {
    match ident_complete(inp) {
        IResult::Done(rest, s) => IResult::Done(rest, String::from_utf8_lossy(s).into_owned()),
        IResult::Incomplete(n) => IResult::Incomplete(n),
        IResult::Error(e) => IResult::Error(e),
    }
}

macro_rules! replace_expr {
    ($_t:tt $sub:expr) => {$sub};
}

macro_rules! count_tts {
    ($($tts:tt)*) => {0usize $(+ replace_expr!($tts 1usize))*};
}

/// Macro to take a spec of a mercurial wire protocol command and generate the
/// code to invoke a parser for it. This works for "regular" commands with a
/// fixed number of named parameters.
macro_rules! command_common {
    // No parameters
    ( $i:expr, $name:expr, $req:ident, $star:expr, $parseparam:expr, { } ) => {
        call!($i, parse_command, $name, $parseparam, $star, |_| Ok($req))
    };

    // One key/parser pair for each parameter
    ( $i:expr, $name:expr, $req:ident, $star:expr, $parseparam:expr,
            { $( ($key:ident, $parser:expr) )+ } ) => {
        call!($i, parse_command, $name, $parseparam, $star+count_tts!( $($key)+ ),
            |kv| Ok($req {
                $( $key: parseval(&kv, stringify!($key), $parser)?, )*
            })
        )
    };
}

macro_rules! command {
    ( $i:expr, $name:expr, $req:ident, $parseparam:expr,
            { $( $key:ident => $parser:expr, )* } ) => {
        command_common!($i, $name, $req, 0, $parseparam, { $(($key, $parser))* } )
    };
}

macro_rules! command_star {
    ( $i:expr, $name:expr, $req:ident, $parseparam:expr,
            { $( $key:ident => $parser:expr, )* } ) => {
        command_common!($i, $name, $req, 1, $parseparam, { $(($key, $parser))* } )
    };
}

/// Parse a non-batched command
pub fn parse(buf: &mut BytesMut) -> Result<Option<Request>> {
    parse_common(buf, params)
}

/// Parse a single batched command (with its parameters in batched form)
pub fn parse_batch(buf: &mut BytesMut) -> Result<Option<Request>> {
    parse_common(buf, batch_params)
}

/// Common parser, generalized over how to parse parameters (either unbatched or
/// batched syntax.)
fn parse_common(
    buf: &mut BytesMut,
    parse_params: fn(&[u8], usize)
        -> IResult<&[u8], HashMap<Vec<u8>, Vec<u8>>>,
) -> Result<Option<Request>> {
    use Request::*;

    let res = {
        let origlen = buf.len();
        let parse_res = alt!(&buf[..],
              command_star!("batch", Batch, parse_params, {
                  cmds => cmdlist,
              })
            | command!("between", Between, parse_params, {
                  pairs => pairlist,
              })
            | command!("branchmap", Branchmap, parse_params, {})
            | command!("branches", Branches, parse_params, {
                  nodes => hashlist,
              })
            | command!("clonebundles", Clonebundles, parse_params, {})
            | command!("capabilities", Capabilities, parse_params, {})
            | command!("changegroup", Changegroup, parse_params, {
                  roots => hashlist,
              })
            | command!("changegroupsubset", Changegroupsubset, parse_params, {
                  heads => hashlist,
                  bases => hashlist,
              })
            | call!(parse_command, "debugwireargs", parse_params, 2+1,
                |kv| Ok(Debugwireargs {
                    one: parseval(&kv, "one", ident_complete)?.to_vec(),
                    two: parseval(&kv, "two", ident_complete)?.to_vec(),
                    all_args: kv,
                }))
            | call!(parse_command, "getbundle", parse_params, 0+1,
                |kv| Ok(Getbundle(GetbundleArgs {
                    // Some params are currently ignored, like:
                    // - obsmarkers
                    // - cg
                    // - cbattempted
                    // If those params are needed, they should be parsed here.
                    heads: parseval_default(&kv, "heads", hashlist)?,
                    common: parseval_default(&kv, "common", hashlist)?,
                    bundlecaps: parseval_default(&kv, "bundlecaps", commavalues)?,
                    listkeys: parseval_default(&kv, "listkeys", commavalues)?,
                })))
            | command!("heads", Heads, parse_params, {})
            | command!("hello", Hello, parse_params, {})
            | command!("listkeys", Listkeys, parse_params, {
                  namespace => ident_string,
              })
            | command!("lookup", Lookup, parse_params, {
                  key => ident_string,
              })
            | command_star!("known", Known, parse_params, {
                  nodes => hashlist,
              })
            | command!("pushkey", Pushkey, parse_params, {
                  namespace => ident_string,
                  key => ident_string,
                  old => nodehash,
                  new => nodehash,
              })
            | command!("streamout", Streamout, parse_params, {})
            | command!("unbundle", Unbundle, parse_params, {
                  heads => hashlist,
              })
        );

        // Turn "rest" into a "consumed" bytecount, so consume it once the
        // borrow from buf has finished.
        match parse_res {
            IResult::Done(rest, val) => Some((origlen - rest.len(), val)),
            IResult::Incomplete(_) => None,
            IResult::Error(err) => {
                bail!(
                Error::with_chain(
                    err,
                    errors::ErrorKind::CommandParse(buf.to_vec()),
                ))
            }
        }
    };

    Ok(res.map(|(consume, val)| {
        let _ = buf.split_to(consume);
        val
    }))
}

/// Test individual combinators
#[cfg(test)]
mod test {
    use super::*;
    use mercurial_types::nodehash;

    #[test]
    fn test_integer() {
        assert_eq!(integer(b"1234 "), IResult::Done(&b" "[..], 1234));
        assert_eq!(integer(b"1234"), IResult::Incomplete(Needed::Unknown));
    }

    #[test]
    fn test_ident() {
        assert_eq!(ident(b"1234 "), IResult::Error(ErrorKind::AlphaNumeric));
        assert_eq!(ident(b" 1234 "), IResult::Error(ErrorKind::AlphaNumeric));
        assert_eq!(ident(b"foo"), IResult::Incomplete(Needed::Unknown));
        assert_eq!(ident(b"foo "), IResult::Done(&b" "[..], &b"foo"[..]));
    }

    #[test]
    fn test_param_star() {
        let p = b"* 0\ntrailer";
        assert_eq!(
            param_star(p),
            IResult::Done(&b"trailer"[..], hashmap! { }));

        let p = b"* 1\n\
                  foo 12\n\
                  hello world!trailer";
        assert_eq!(
            param_star(p),
            IResult::Done(&b"trailer"[..], hashmap! {
                b"foo".to_vec() => b"hello world!".to_vec(),
            }
        ));

        let p = b"* 2\n\
                  foo 12\n\
                  hello world!\
                  bar 4\n\
                  bloptrailer";
        assert_eq!(
            param_star(p),
            IResult::Done(&b"trailer"[..], hashmap! {
                b"foo".to_vec() => b"hello world!".to_vec(),
                b"bar".to_vec() => b"blop".to_vec(),
            }
        ));

        // no trailer
        let p = b"* 0\n";
        assert_eq!(
            param_star(p),
            IResult::Done(&b""[..], hashmap! { }));

        let p = b"* 1\n\
                  foo 12\n\
                  hello world!";
        assert_eq!(
            param_star(p),
            IResult::Done(&b""[..], hashmap! {
                b"foo".to_vec() => b"hello world!".to_vec(),
            }
        ));
    }

    #[test]
    fn test_param_kv() {
        let p = b"foo 12\n\
                  hello world!trailer";
        assert_eq!(
            param_kv(p),
            IResult::Done(&b"trailer"[..], hashmap! {
                b"foo".to_vec() => b"hello world!".to_vec(),
            }));

        let p = b"foo 12\n\
                  hello world!";
        assert_eq!(
            param_kv(p),
            IResult::Done(&b""[..], hashmap! {
                b"foo".to_vec() => b"hello world!".to_vec(),
            }));
    }

    #[test]
    fn test_params() {
        let p = b"bar 12\n\
                  hello world!\
                  foo 7\n\
                  blibble\
                  very_long_key_no_data 0\n\
                  is_ok 1\n\
                  y\n\
                  badly formatted thing ";

        match params(p, 1) {
            IResult::Done(_, v) => {
                assert_eq!(v, hashmap! {
                b"bar".to_vec() => b"hello world!".to_vec(),
            })
            }
            bad => panic!("bad result {:?}", bad),
        }

        match params(p, 2) {
            IResult::Done(_, v) => {
                assert_eq!(v, hashmap! {
                b"bar".to_vec() => b"hello world!".to_vec(),
                b"foo".to_vec() => b"blibble".to_vec(),
            })
            }
            bad => panic!("bad result {:?}", bad),
        }

        match params(p, 4) {
            IResult::Done(b"\nbadly formatted thing ", v) => {
                assert_eq!(v, hashmap! {
                b"bar".to_vec() => b"hello world!".to_vec(),
                b"foo".to_vec() => b"blibble".to_vec(),
                b"very_long_key_no_data".to_vec() => b"".to_vec(),
                b"is_ok".to_vec() => b"y".to_vec(),
            })
            }
            bad => panic!("bad result {:?}", bad),
        }

        match params(p, 5) {
            IResult::Error(ErrorKind::Alt) => (),
            bad => panic!("bad result {:?}", bad),
        }

        match params(&p[..3], 1) {
            IResult::Incomplete(_) => (),
            bad => panic!("bad result {:?}", bad),
        }

        for l in 0..p.len() {
            match params(&p[..l], 4) {
                IResult::Incomplete(_) => (),
                IResult::Done(remain, ref kv) => {
                    assert_eq!(kv.len(), 4);
                    assert!(b"\nbadly formatted thing ".starts_with(remain),
                        "remain \"{:?}\"", remain);
                }
                bad => panic!("bad result l {} bad {:?}", l, bad),
            }
        }
    }

    #[test]
    fn test_params_star() {
        let star = b"* 1\n\
                     foo 0\n\
                     bar 0\n";
        match params(star, 2) {
            IResult::Incomplete(_) => panic!("unexpectedly incomplete"),
            IResult::Done(remain, kv) => {
                assert_eq!(remain, b"");
                assert_eq!(kv, hashmap! {
                    b"foo".to_vec() => vec!{},
                    b"bar".to_vec() => vec!{},
                });
            }
            IResult::Error(err) => panic!("unexpected error {:?}", err),
        }

        let star = b"* 2\n\
                     foo 0\n\
                     plugh 0\n\
                     bar 0\n";
        match params(star, 2) {
            IResult::Incomplete(_) => panic!("unexpectedly incomplete"),
            IResult::Done(remain, kv) => {
                assert_eq!(remain, b"");
                assert_eq!(kv, hashmap! {
                    b"foo".to_vec() => vec!{},
                    b"bar".to_vec() => vec!{},
                    b"plugh".to_vec() => vec!{},
                });
            }
            IResult::Error(err) => panic!("unexpected error {:?}", err),
        }

        let star = b"* 0\n\
                     bar 0\n";
        match params(star, 2) {
            IResult::Incomplete(_) => panic!("unexpectedly incomplete"),
            IResult::Done(remain, kv) => {
                assert_eq!(remain, b"");
                assert_eq!(kv, hashmap! {
                    b"bar".to_vec() => vec!{},
                });
            }
            IResult::Error(err) => panic!("unexpected error {:?}", err),
        }

        match params(&star[..4], 2) {
            IResult::Incomplete(_) => (),
            IResult::Done(remain, kv) => panic!("unexpected Done remain {:?} kv {:?}", remain, kv),
            IResult::Error(err) => panic!("unexpected error {:?}", err),
        }
    }

    #[test]
    fn test_batch_param_escaped() {
        let p = b"foo=b:ear";

        assert_eq!(
            batch_param_escaped(p),
            IResult::Done(&b""[..], (b"foo".to_vec(), b"b=ar".to_vec())));
    }

    #[test]
    fn test_batch_params() {
        let p = b"foo=bar";

        assert_eq!(batch_params(p, 0), IResult::Done(&b""[..], hashmap!{
            b"foo".to_vec() => b"bar".to_vec(),
        }));

        let p = b"foo=bar,biff=bop,esc:c:o:s:e=esc:c:o:s:e";

        assert_eq!(batch_params(p, 0), IResult::Done(&b""[..], hashmap!{
            b"foo".to_vec() => b"bar".to_vec(),
            b"biff".to_vec() => b"bop".to_vec(),
            b"esc:,;=".to_vec() => b"esc:,;=".to_vec(),
        }));

        let p = b"";

        assert_eq!(batch_params(p, 0), IResult::Done(&b""[..], hashmap!{
        }));
    }

    #[test]
    fn test_nodehash() {
        assert_eq!(
            nodehash(b"0000000000000000000000000000000000000000"),
            IResult::Done(&b""[..], nodehash::NULL_HASH));

        assert_eq!(
            nodehash(b"000000000000000000000000000000x000000000"),
            IResult::Error(ErrorKind::MapRes));

        assert_eq!(
            nodehash(b"000000000000000000000000000000000000000"),
            IResult::Incomplete(Needed::Size(40)));
    }

    #[test]
    fn test_parseval_extra_characters() {
        let kv = hashmap! {
        b"foo".to_vec() => b"0000000000000000000000000000000000000000extra".to_vec(),
        };
        match parseval(&kv, "foo", hashlist) {
            Err(_) => (),
            _ => {
                panic!("Paramval parse failed: Did not raise an error for param\
                         with trailing characters.")
            }
        }
    }

    #[test]
    fn test_parseval_default_extra_characters() {
        let kv = hashmap! {
        b"foo".to_vec() => b"0000000000000000000000000000000000000000extra".to_vec(),
        };
        match parseval_default(&kv, "foo", hashlist) {
            Err(_) => (),
            _ => {
                panic!("paramval_default parse failed: Did not raise an error for param\
                         with trailing characters.")
            }
        }
    }

    #[test]
    fn test_pair() {
        let p =
            b"0000000000000000000000000000000000000000-0000000000000000000000000000000000000000";
        assert_eq!(
            pair(p),
            IResult::Done(&b""[..], (nodehash::NULL_HASH, nodehash::NULL_HASH))
        );

        assert_eq!(
            pair(&p[..80]),
            IResult::Incomplete(Needed::Size(81))
        );

        assert_eq!(
            pair(&p[..41]),
            IResult::Incomplete(Needed::Size(81))
        );

        assert_eq!(
            pair(&p[..40]),
            IResult::Incomplete(Needed::Size(41))
        );
    }

    #[test]
    fn test_pairlist() {
        let p =
            b"0000000000000000000000000000000000000000-0000000000000000000000000000000000000000 \
              0000000000000000000000000000000000000000-0000000000000000000000000000000000000000";
        assert_eq!(
            pairlist(p),
            IResult::Done(&b""[..], vec! {
                (nodehash::NULL_HASH, nodehash::NULL_HASH),
                (nodehash::NULL_HASH, nodehash::NULL_HASH),
            })
        );

        let p =
            b"0000000000000000000000000000000000000000-0000000000000000000000000000000000000000";
        assert_eq!(
            pairlist(p),
            IResult::Done(&b""[..], vec! {
                (nodehash::NULL_HASH, nodehash::NULL_HASH),
            })
        );
    }

    #[test]
    fn test_hashlist() {
        let p =
            b"0000000000000000000000000000000000000000 0000000000000000000000000000000000000000 \
              0000000000000000000000000000000000000000 0000000000000000000000000000000000000000";
        assert_eq!(
            hashlist(p),
            IResult::Done(&b""[..], vec! {
                nodehash::NULL_HASH,
                nodehash::NULL_HASH,
                nodehash::NULL_HASH,
                nodehash::NULL_HASH,
            })
        );

        let p = b"0000000000000000000000000000000000000000";
        assert_eq!(
            hashlist(p),
            IResult::Done(&b""[..], vec! {
                nodehash::NULL_HASH,
            })
        );
    }

    #[test]
    fn test_commavalues() {
        // Empty list
        let p = b"";
        assert_eq!(commavalues(p), IResult::Done(&b""[..], vec![]));

        // Single entry
        let p = b"abc";
        assert_eq!(commavalues(p), IResult::Done(&b""[..], vec![b"abc".to_vec()]));

        // Multiple entries
        let p = b"123,abc,test,456";
        assert_eq!(
            commavalues(p),
            IResult::Done(&b""[..], vec![
                b"123".to_vec(),
                b"abc".to_vec(),
                b"test".to_vec(),
                b"456".to_vec(),
            ])
        );



    }

    #[test]
    fn test_cmd() {
        let p = b"foo bar";

        assert_eq!(cmd(p), IResult::Done(&b""[..], (b"foo".to_vec(), b"bar".to_vec())));

        let p = b"noparam ";
        assert_eq!(cmd(p), IResult::Done(&b""[..], (b"noparam".to_vec(), b"".to_vec())));
    }

    #[test]
    fn test_cmdlist() {
        let p = b"foo bar";

        assert_eq!(cmdlist(p), IResult::Done(&b""[..], vec! {
            (b"foo".to_vec(), b"bar".to_vec()),
        }));

        let p = b"foo bar;biff blop";

        assert_eq!(cmdlist(p), IResult::Done(&b""[..], vec! {
            (b"foo".to_vec(), b"bar".to_vec()),
            (b"biff".to_vec(), b"blop".to_vec()),
        }));
    }
}

/// Test parsing each command
#[cfg(test)]
mod test_parse {
    use super::*;
    use std::fmt::Display;

    fn hash_ones() -> NodeHash {
        "1111111111111111111111111111111111111111".parse().unwrap()
    }

    fn hash_twos() -> NodeHash {
        "2222222222222222222222222222222222222222".parse().unwrap()
    }

    fn hash_threes() -> NodeHash {
        "3333333333333333333333333333333333333333".parse().unwrap()
    }

    fn hash_fours() -> NodeHash {
        "4444444444444444444444444444444444444444".parse().unwrap()
    }

    /// Common code for testing parsing:
    /// - check all truncated inputs return "Ok(None)"
    /// - complete inputs return the expected result, and leave any remainder in
    ///    the input buffer.
    fn test_parse<I: AsRef<[u8]> + Display>(inp: I, exp: Request) {
        let inbytes = inp.as_ref();

        // check for short inputs
        for l in 0..inbytes.len() - 1 {
            let mut buf = BytesMut::from(inbytes[0..l].to_vec());
            match parse(&mut buf) {
                Ok(None) => (),
                Ok(Some(val)) => {
                    panic!("BAD PASS: inp >>{}<< len {} passed unexpectedly val {:?}",
                        inp, l, val)
                }
                Err(err) => {
                    panic!("BAD FAIL: inp >>{}<< len {} failed {:?} (not incomplete)",
                        inp, l, err)
                }
            };
        }

        // check for exact and extra
        let extra = b"extra";
        for l in 0..extra.len() {
            let mut buf = BytesMut::from(inbytes.to_vec());
            buf.extend_from_slice(&extra[0..l]);
            match parse(&mut buf) {
                Ok(Some(val)) => assert_eq!(val, exp),
                Ok(None) => panic!("BAD INCOMPLETE: inp >>{}<< extra {} incomplete", inp, l),
                Err(err) => {
                    panic!("BAD FAIL: inp >>{}<< extra {} failed {:?} (not incomplete)",
                        inp, l, err)
                }
            };
            assert_eq!(&*buf, &extra[0..l]);
        }
    }

    #[test]
    fn test_parse_batch() {
        let inp = "batch\n\
                   * 0\n\
                   cmds 6\n\
                   hello ";

        test_parse(
            inp,
            Request::Batch {
                cmds: vec! { (b"hello".to_vec(), vec!{})},
            },
        )
    }

    #[test]
    fn test_parse_between() {
        let inp = "between\n\
                   pairs 163\n\
                   1111111111111111111111111111111111111111-2222222222222222222222222222222222222222 \
                   3333333333333333333333333333333333333333-4444444444444444444444444444444444444444";
        test_parse(
            inp,
            Request::Between {
                pairs: vec! {
                    (hash_ones(), hash_twos()),
                    (hash_threes(), hash_fours()),
                },
            },
        );
    }

    #[test]
    fn test_parse_branchmap() {
        let inp = "branchmap\n";

        test_parse(inp, Request::Branchmap {});
    }

    #[test]
    fn test_parse_branches() {
        let inp = "branches\n\
                   nodes 163\n\
                   1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 \
                   3333333333333333333333333333333333333333 4444444444444444444444444444444444444444";
        test_parse(
            inp,
            Request::Branches {
                nodes: vec! {
                    hash_ones(),
                    hash_twos(),
                    hash_threes(),
                    hash_fours(),
                 },
            },
        );
    }

    #[test]
    fn test_parse_clonebundles() {
        let inp = "clonebundles\n";

        test_parse(inp, Request::Clonebundles {});
    }

    #[test]
    fn test_parse_capabilities() {
        let inp = "capabilities\n";

        test_parse(inp, Request::Capabilities {});
    }

    #[test]
    fn test_parse_changegroup() {
        let inp = "changegroup\n\
                   roots 81\n\
                   1111111111111111111111111111111111111111 2222222222222222222222222222222222222222";

        test_parse(
            inp,
            Request::Changegroup {
                roots: vec! { hash_ones(), hash_twos() },
            },
        );
    }

    #[test]
    fn test_parse_changegroupsubset() {
        let inp = "changegroupsubset\n\
                   heads 40\n\
                   1111111111111111111111111111111111111111\
                   bases 81\n\
                   2222222222222222222222222222222222222222 3333333333333333333333333333333333333333";

        test_parse(
            inp,
            Request::Changegroupsubset {
                heads: vec! {
                    hash_ones(),
                },
                bases: vec! {
                    hash_twos(),
                    hash_threes(),
                },
            },
        );
    }

    #[test]
    fn test_parse_debugwireargs() {
        let inp = "debugwireargs\n\
                   * 2\n\
                   three 5\nTHREE\
                   empty 0\n\
                   one 3\nONE\
                   two 3\nTWO";
        test_parse(
            inp,
            Request::Debugwireargs {
                one: b"ONE".to_vec(),
                two: b"TWO".to_vec(),
                all_args: hashmap! {
                    b"one".to_vec() => b"ONE".to_vec(),
                    b"two".to_vec() => b"TWO".to_vec(),
                    b"three".to_vec() => b"THREE".to_vec(),
                    b"empty".to_vec() => vec![],
                },
            },
        );
    }

    #[test]
    fn test_parse_getbundle() {
        // with no arguments
        let inp = "getbundle\n\
                   * 0\n";

        test_parse(
            inp,
            Request::Getbundle(GetbundleArgs {
                heads: vec![],
                common: vec![],
                bundlecaps: vec![],
                listkeys: vec![],
            }),
        );

        // with arguments
        let inp = "getbundle\n\
                   * 5\n\
                   heads 40\n\
                   1111111111111111111111111111111111111111\
                   common 81\n\
                   2222222222222222222222222222222222222222 3333333333333333333333333333333333333333\
                   bundlecaps 14\n\
                   cap1,CAP2,cap3\
                   listkeys 9\n\
                   key1,key2\
                   extra 5\n\
                   extra";
        test_parse(
            inp,
            Request::Getbundle(GetbundleArgs {
                heads: vec![hash_ones()],
                common: vec![hash_twos(), hash_threes()],
                bundlecaps: vec![b"cap1".to_vec(), b"CAP2".to_vec(), b"cap3".to_vec()],
                listkeys: vec![b"key1".to_vec(), b"key2".to_vec()],
            }),
        );
    }

    #[test]
    fn test_parse_heads() {
        let inp = "heads\n";

        test_parse(inp, Request::Heads {});
    }

    #[test]
    fn test_parse_hello() {
        let inp = "hello\n";

        test_parse(inp, Request::Hello {});
    }

    #[test]
    fn test_parse_listkeys() {
        let inp = "listkeys\n\
                   namespace 9\n\
                   bookmarks";

        test_parse(
            inp,
            Request::Listkeys {
                namespace: "bookmarks".to_string(),
            },
        );
    }

    #[test]
    fn test_parse_lookup() {
        let inp = "lookup\n\
                   key 9\n\
                   bookmarks";

        test_parse(
            inp,
            Request::Lookup {
                key: "bookmarks".to_string(),
            },
        );
    }

    #[test]
    fn test_parse_known() {
        let inp = "known\n\
                   * 0\n\
                   nodes 40\n\
                   1111111111111111111111111111111111111111";

        test_parse(
            inp,
            Request::Known {
                nodes: vec! { hash_ones() },
            },
        );
    }

    #[test]
    fn test_parse_pushkey() {
        let inp = "pushkey\n\
                   namespace 9\n\
                   bookmarks\
                   key 6\n\
                   foobar\
                   old 40\n\
                   1111111111111111111111111111111111111111\
                   new 40\n\
                   2222222222222222222222222222222222222222";

        test_parse(
            inp,
            Request::Pushkey {
                namespace: "bookmarks".to_string(),
                key: "foobar".to_string(),
                old: hash_ones(),
                new: hash_twos(),
            },
        );
    }

    #[test]
    fn test_parse_streamout() {
        let inp = "streamout\n";

        test_parse(inp, Request::Streamout {});
    }

    #[test]
    fn test_parse_unbundle() {
        let inp = "unbundle\n\
                   heads 40\n\
                   1111111111111111111111111111111111111111";

        test_parse(
            inp,
            Request::Unbundle {
                heads: vec! { hash_ones() },
            },
        );
    }

    #[test]
    fn test_batch_parse_heads() {
        let mut inp = BytesMut::from(b"heads\n".to_vec());

        match parse_batch(&mut inp) {
            Ok(Some(res)) => assert_eq!(res, Request::Heads {}),
            Ok(None) => panic!("unexpected incomplete input"),
            Err(err) => panic!("failed with {:?}", err),
        }
    }

    #[test]
    fn test_parse_batch_heads() {
        let inp = "batch\n\
                   * 0\n\
                   cmds 100\n\
                   heads ;\
                   known nodes=ee07e8c0780b5059e874c5b0dbcab2278fde2a14 \
                   3243aa153e20a170cd2c7441c595c44a9b087f5b";

        test_parse(
            inp,
            Request::Batch {
                cmds: vec! {
                (b"heads".to_vec(), vec!{}),
                (b"known".to_vec(),
                    b"nodes=ee07e8c0780b5059e874c5b0dbcab2278fde2a14 \
                      3243aa153e20a170cd2c7441c595c44a9b087f5b".to_vec()),
            },
            },
        );
    }

}
