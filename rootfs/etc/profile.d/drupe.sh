# Commands of installed drupe tools (`drupe list`) come before the system's:
# a tool may replace a command of the image for the user, never for the
# system itself, which doesn't read this file.
case ":${PATH}:" in
    *:/run/drupe/bin:*) ;;
    *) PATH="/run/drupe/bin:${PATH}" ;;
esac
