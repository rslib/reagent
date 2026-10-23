"""A small command line parser: long and short options, flags and counters."""


class ArgError(Exception):
    pass


class Option:
    def __init__(self, name, short=None, takes_value=False, count=False, default=None, kind=str):
        self.name = name
        self.short = short
        self.takes_value = takes_value
        self.count = count
        self.default = default
        self.kind = kind


def default_options():
    """The options the tool knows.

    --retries takes a number and defaults to 3, --jobs defaults to 1,
    -v can repeat to raise the verbosity and defaults to 0.
    """
    return [
        Option("jobs", short="j", takes_value=True, default=1, kind=int),
        Option("retries", short="r", takes_value=True, default=0, kind=int),
        Option("verbose", short="v", count=True, default=0),
        Option("dry_run", short="n", default=False),
        Option("output", short="o", takes_value=True, default=None),
    ]


def _find(options, key, short):
    for opt in options:
        if short and opt.short == key:
            return opt
        if not short and opt.name == key.replace("-", "_"):
            return opt
    raise ArgError("unknown option: " + ("-" if short else "--") + key)


def _store(result, opt, value):
    if opt.count:
        result[opt.name] += 1
    elif opt.takes_value:
        try:
            result[opt.name] = opt.kind(value)
        except ValueError:
            raise ArgError("bad value for %s: %s" % (opt.name, value)) from None
    else:
        result[opt.name] = True


def parse_args(argv, options=None):
    """Return (options dict, positional list)."""
    options = options if options is not None else default_options()
    result = {opt.name: opt.default for opt in options}
    positional = []
    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg.startswith("--") and len(arg) > 2:
            key, eq, value = arg[2:].partition("=")
            opt = _find(options, key, short=False)
            if opt.takes_value and not eq:
                i += 1
                if i >= len(argv):
                    raise ArgError("missing value for --" + key)
                value = argv[i]
            _store(result, value, opt)
        elif arg.startswith("-") and len(arg) > 1:
            letters = arg[1:]
            for n, letter in enumerate(letters):
                opt = _find(options, letter, short=True)
                if opt.takes_value:
                    rest = letters[n + 1:]
                    if rest:
                        value = rest
                    else:
                        i += 1
                        if i >= len(argv):
                            raise ArgError("missing value for -" + letter)
                        value = argv[i]
                    _store(result, opt, value)
                    break
                _store(result, opt, None)
        else:
            positional.append(arg)
        i += 1
    return result, positional


def usage(options=None):
    lines = ["usage: tool [options] [files...]", ""]
    for opt in options if options is not None else default_options():
        flag = "--" + opt.name.replace("_", "-")
        if opt.short:
            flag = "-%s, %s" % (opt.short, flag)
        if opt.takes_value:
            flag += " <value>"
        lines.append("  %-24s default: %s" % (flag, opt.default))
    return "\n".join(lines)
