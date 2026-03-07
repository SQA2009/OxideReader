import sys
import fitz  # PyMuPDF
from PyQt5.QtWidgets import (
    QApplication, QMainWindow, QFileDialog, QScrollArea, QLabel,
    QVBoxLayout, QSlider, QStatusBar, QLineEdit, QHBoxLayout, QWidget
)
from PyQt5.QtCore import Qt, QThread, pyqtSignal, QMutex, QMutexLocker, QPoint, QTimer
from PyQt5.QtGui import QImage, QPixmap, QIntValidator


class RenderPageThread(QThread):
    rendered = pyqtSignal(int, QPixmap)  # Signal emitted when a page is rendered

    def __init__(self, document, page_number, zoom_factor):
        super().__init__()
        self.document = document
        self.page_number = page_number
        self.zoom_factor = zoom_factor

    def run(self):
        try:
            print(f"[DEBUG] Starting render for page {self.page_number} at zoom {self.zoom_factor * 100:.0f}%")
            page = self.document[self.page_number]
            matrix = fitz.Matrix(self.zoom_factor, self.zoom_factor)
            pix = page.get_pixmap(matrix=matrix, alpha=False)

            img = QImage(pix.samples, pix.width, pix.height, pix.stride, QImage.Format_RGB888)
            pixmap = QPixmap.fromImage(img)
            self.rendered.emit(self.page_number, pixmap)
            print(f"[DEBUG] Finished render for page {self.page_number}")
        except Exception as e:
            print(f"[ERROR] Error rendering page {self.page_number}: {e}")


class PDFViewer(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("PDF Viewer - Debug Mode")
        self.resize(1024, 768)

        # Initial state
        self.current_document = None
        self.zoom_factor = 1.0
        self.base_zoom_factor = 1.0
        self.max_zoom_factor = 8.5  # Limit zoom factor to prevent crashes
        self.page_spacing = 20
        self.page_widgets = []
        self.page_positions = []
        self.render_threads = []
        self.rendering_queue = []
        self.rendering_in_progress = set()
        self.render_mutex = QMutex()
        self.last_scroll_value = 0
        self.page_cache = {}
        self.debounce_timer = QTimer()
        self.debounce_timer.setSingleShot(True)
        self.debounce_timer.timeout.connect(self.reload_visible_pages_with_zoom)

        # Scroll area
        self.scroll_area = QScrollArea()
        self.scroll_area.setWidgetResizable(True)
        self.setCentralWidget(self.scroll_area)

        # Content widget
        self.content_widget = QWidget()
        self.scroll_area.setWidget(self.content_widget)
        self.content_layout = QVBoxLayout(self.content_widget)
        self.content_layout.setSpacing(self.page_spacing)
        self.content_layout.setAlignment(Qt.AlignHCenter)

        # Styling
        self.content_widget.setStyleSheet("background-color: rgba(231,236,241,255); border: none;")

        # Status bar
        self.status_bar = QStatusBar()
        self.setStatusBar(self.status_bar)

        self.page_label = QLabel("Page: -/-")
        self.status_bar.addWidget(self.page_label)

        # Connect scroll event to page update
        self.scroll_area.verticalScrollBar().valueChanged.connect(self.handle_scroll)

        # Zoom controls
        self.zoom_layout = QHBoxLayout()
        self.zoom_slider = QSlider(Qt.Horizontal, self)
        self.zoom_slider.setRange(10, int(self.max_zoom_factor * 100))
        self.zoom_slider.setValue(100)
        self.zoom_slider.valueChanged.connect(self.on_zoom_slider_changed)

        self.zoom_input = QLineEdit("100", self)
        self.zoom_input.setFixedWidth(60)
        self.zoom_input.setValidator(QIntValidator(10, int(self.max_zoom_factor * 100)))
        self.zoom_input.returnPressed.connect(self.on_zoom_input_changed)

        self.zoom_layout.addWidget(QLabel("Zoom:"))
        self.zoom_layout.addWidget(self.zoom_slider)
        self.zoom_layout.addWidget(self.zoom_input)

        zoom_container = QWidget()
        zoom_container.setLayout(self.zoom_layout)
        self.status_bar.addPermanentWidget(zoom_container)

        # Menu
        self.init_menu()

    def init_menu(self):
        menu = self.menuBar()
        file_menu = menu.addMenu("File")

        open_action = file_menu.addAction("Open PDF")
        open_action.setShortcut("Ctrl+O")
        open_action.triggered.connect(self.open_pdf)

    def open_pdf(self):
        file_name, _ = QFileDialog.getOpenFileName(self, "Open PDF", "", "PDF Files (*.pdf)")
        if file_name:
            print(f"[DEBUG] Opening PDF file: {file_name}")
            try:
                self.current_document = fitz.open(file_name)
            except Exception as e:
                print(f"[ERROR] Failed to open PDF: {e}")
                return

            # Reset state
            for i in reversed(range(self.content_layout.count())):
                widget = self.content_layout.itemAt(i).widget()
                if widget:
                    widget.deleteLater()

            self.page_widgets.clear()
            self.page_positions.clear()
            self.render_threads.clear()
            self.rendering_queue.clear()
            self.rendering_in_progress.clear()
            self.page_cache.clear()
            self.zoom_factor = 1.0
            self.zoom_slider.setValue(100)
            self.zoom_input.setText("100")

            # Reset scroll position
            self.scroll_area.verticalScrollBar().setValue(0)
            self.last_scroll_value = 0

            self.load_pages()

    def load_pages(self):
        """Load pages and create placeholders."""
        if not self.current_document:
            return

        total_pages = len(self.current_document)
        print(f"[DEBUG] Total pages in document: {total_pages}")
        self.page_label.setText(f"Page: 1/{total_pages}")

        for i in range(total_pages):
            label = QLabel()
            label.setAlignment(Qt.AlignCenter)
            self.content_layout.addWidget(label)
            self.page_widgets.append(label)
            self.page_positions.append(QPoint(0, 0))  # Placeholder for page positions

            # Render and cache at base zoom factor
            self.render_page(i, self.base_zoom_factor)

        self.update_visible_page()
        self.queue_render_visible_pages()

    def render_page(self, page_number, zoom_factor):
        """Render a page and cache it."""
        if (page_number, zoom_factor) in self.page_cache:
            return

        with QMutexLocker(self.render_mutex):
            if page_number not in self.rendering_in_progress:
                self.rendering_in_progress.add(page_number)
            else:
                return

        thread = RenderPageThread(self.current_document, page_number, zoom_factor)
        thread.rendered.connect(self.handle_render_finished)
        thread.start()
        self.render_threads.append(thread)

    def update_visible_page(self):
        """Update the page number indicator based on the currently visible page."""
        if not self.page_widgets or not self.current_document:
            return

        scroll_bar = self.scroll_area.verticalScrollBar()
        scroll_center = scroll_bar.value() + self.scroll_area.height() // 2

        closest_page = 0
        closest_distance = float('inf')
        for i, widget in enumerate(self.page_widgets):
            widget_top = widget.pos().y()
            widget_bottom = widget_top + widget.height()
            widget_center = (widget_top + widget_bottom) // 2

            distance = abs(widget_center - scroll_center)
            if distance < closest_distance:
                closest_distance = distance
                closest_page = i

        total_pages = len(self.current_document)
        self.page_label.setText(f"Page: {closest_page + 1}/{total_pages}")
        print(f"[DEBUG] Currently visible page: {closest_page + 1}")

        self.queue_render_visible_pages()

    def queue_render_visible_pages(self):
        """Queue rendering of only the visible pages."""
        if not self.current_document:
            return

        with QMutexLocker(self.render_mutex):
            visible_pages = self.get_visible_pages()
            self.rendering_queue = [page for page in visible_pages if page not in self.rendering_in_progress]

        self.process_next_render()

    def get_visible_pages(self):
        """Get the indices of currently visible pages."""
        scroll_bar = self.scroll_area.verticalScrollBar()
        scroll_top = scroll_bar.value()
        scroll_bottom = scroll_top + self.scroll_area.height()

        visible_pages = []
        for i, widget in enumerate(self.page_widgets):
            widget_top = widget.pos().y()
            widget_bottom = widget_top + widget.height()
            if widget_bottom >= scroll_top and widget_top <= scroll_bottom:
                visible_pages.append(i)
        
        print(f"[DEBUG] Visible pages: {visible_pages}")
        return visible_pages

    def process_next_render(self):
        """Process the next page in the render queue."""
        with QMutexLocker(self.render_mutex):
            if not self.rendering_queue:
                return

            next_page = self.rendering_queue.pop(0)
            self.rendering_in_progress.add(next_page)

        print(f"[DEBUG] Queued rendering for page {next_page}")
        thread = RenderPageThread(self.current_document, next_page, self.zoom_factor)
        thread.rendered.connect(self.handle_render_finished)
        thread.start()
        self.render_threads.append(thread)

    def handle_render_finished(self, page_number, pixmap):
        """Handle the completion of page rendering and trigger the next one."""
        with QMutexLocker(self.render_mutex):
            if page_number in self.rendering_in_progress:
                self.rendering_in_progress.remove(page_number)
                print(f"[DEBUG] Removed page {page_number} from rendering in progress")
            else:
                print(f"[WARN] Tried to remove page {page_number} from rendering in progress but it wasn't found")

        if 0 <= page_number < len(self.page_widgets):
            self.page_widgets[page_number].setPixmap(pixmap)
            self.page_cache[(page_number, self.zoom_factor)] = pixmap
        print(f"[DEBUG] Finished rendering for page {page_number}")

        # Start the next render
        self.process_next_render()

    def handle_scroll(self):
        """Handle scroll events to update the visible page and maintain scroll position during zoom."""
        self.update_visible_page()
        self.last_scroll_value = self.scroll_area.verticalScrollBar().value()

    def on_zoom_slider_changed(self):
        """Handle zoom slider changes."""
        self.zoom_factor = self.zoom_slider.value() / 100.0
        self.zoom_input.setText(str(self.zoom_slider.value()))
        print(f"[DEBUG] Zoom changed via slider to {self.zoom_slider.value()}%")
        self.apply_visual_zoom()

    def on_zoom_input_changed(self):
        """Handle zoom input changes."""
        value = int(self.zoom_input.text())
        if value > self.max_zoom_factor * 100:
            value = int(self.max_zoom_factor * 100)
            self.zoom_input.setText(str(value))
        self.zoom_slider.setValue(value)
        self.zoom_factor = value / 100.0
        print(f"[DEBUG] Zoom changed via input to {value}%")
        self.apply_visual_zoom()

    def apply_visual_zoom(self):
        """Apply visual zoom transformation to the cached images."""
        for (page_number, _), pixmap in self.page_cache.items():
            scaled_pixmap = pixmap.scaled(pixmap.size() * self.zoom_factor, Qt.KeepAspectRatio, Qt.SmoothTransformation)
            self.page_widgets[page_number].setPixmap(scaled_pixmap)
        self.debounce_timer.start(300)  # Debounce for 300ms

    def reload_visible_pages_with_zoom(self):
        """Reload only the visible pages at the current zoom level."""
        print(f"[DEBUG] Reloading visible pages with zoom {self.zoom_factor * 100:.0f}%")
        self.queue_render_visible_pages()
        self.scroll_area.verticalScrollBar().setValue(self.last_scroll_value)


if __name__ == "__main__":
    app = QApplication(sys.argv)
    viewer = PDFViewer()
    viewer.show()
    sys.exit(app.exec_())