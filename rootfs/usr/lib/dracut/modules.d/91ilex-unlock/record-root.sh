#!/bin/sh
# Missing method evidence stays unknown; it must never prevent recovery boot.
/usr/bin/ilex-enroll record-root --root /sysroot || :
