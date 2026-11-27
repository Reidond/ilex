#pragma once

#include <QtCore/QString>

// Draw the first visible Qt Quick window to a PNG file, at the window's own
// device pixel ratio (a window at 150% gives a file 150% as large). Returns an
// empty string on success, else what went wrong. Call it on the Qt thread.
QString ilexGrabWindow(const QString &path);
