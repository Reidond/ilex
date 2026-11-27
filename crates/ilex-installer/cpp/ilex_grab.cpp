#include "ilex_grab.h"

#include <QtGui/QGuiApplication>
#include <QtGui/QImage>
#include <QtQuick/QQuickWindow>

QString ilexGrabWindow(const QString &path)
{
    const auto windows = QGuiApplication::allWindows();
    for (QWindow *window : windows) {
        auto *quick = qobject_cast<QQuickWindow *>(window);
        if (!quick || !quick->isVisible()) {
            continue;
        }
        const QImage image = quick->grabWindow();
        if (image.isNull()) {
            return QStringLiteral("the window drew nothing");
        }
        if (!image.save(path, "PNG")) {
            return QStringLiteral("could not write ") + path;
        }
        return QString();
    }
    return QStringLiteral("no visible Qt Quick window");
}

QString ilexItemText(QObject *item)
{
    return item ? item->property("text").toString() : QString();
}

void ilexSetItemText(QObject *item, const QString &text)
{
    if (item) {
        item->setProperty("text", text);
    }
}
