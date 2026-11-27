#pragma once

#include <QtCore/QObject>
#include <QtCore/QString>

// Draw the first visible Qt Quick window to a PNG file, at the window's own
// device pixel ratio (a window at 150% gives a file 150% as large). Returns an
// empty string on success, else what went wrong. Call it on the Qt thread.
QString ilexGrabWindow(const QString &path);

// The `text` property of a QML text field (empty when it has none), and setting
// it. Call them on the Qt thread: the installer reads a password field here so
// that the text is not passed through a JavaScript string for each keystroke.
QString ilexItemText(QObject *item);
void ilexSetItemText(QObject *item, const QString &text);
