;;; gnosis-test-tl.el --- Focused tests for Gnosis tabulated lists -*- lexical-binding: t; -*-

(require 'ert)
(require 'tabulated-list)
(require 'gnosis-tl)

(defun gnosis-test-tl--setup (entries)
  "Initialize a compact tabulated list with ENTRIES."
  (tabulated-list-mode)
  (setq tabulated-list-format [("Name" 20 t) ("Value" 10 t)]
        tabulated-list-padding 2
        tabulated-list-entries entries)
  (tabulated-list-init-header)
  (gnosis-tl-print))

(ert-deftest gnosis-tl-format-line-retains-row-properties ()
  (let* ((tabulated-list-padding 2)
         (columns ["hello" "world"])
         (specs (gnosis-tl--column-specs
                 [("Name" 20 t) ("Value" 10 t)]))
         (line (gnosis-tl--format-line "row" columns specs)))
    (should (equal "row" (get-text-property 0 'tabulated-list-id line)))
    (should (equal columns
                   (get-text-property 0 'tabulated-list-entry line)))
    (should (string-suffix-p "\n" line))))

(ert-deftest gnosis-tl-ascii-fast-path-honors-right-alignment ()
  (should
   (equal "L       R Z\n"
          (gnosis-tl-render-lines
           '(("row" ["L" "R" "Z"]))
           [("Left" 4 nil)
            ("Right" 4 nil :right-align t)
            ("Last" 3 nil)]
           0))))

(ert-deftest gnosis-tl-final-column-honors-right-alignment ()
  (let ((format [("Name" 1 nil) ("Value" 4 nil :right-align t)]))
    (should
     (equal "A    X\nB    λ\n"
            (gnosis-tl-render-lines
             '(("a" ["A" "X"]) ("b" ["B" "λ"])) format 0)))
    (with-temp-buffer
      (tabulated-list-mode)
      (setq tabulated-list-format format
            tabulated-list-padding 0
            tabulated-list-entries '(("a" ["A" "X"])))
      (tabulated-list-init-header)
      (gnosis-tl-print)
      (gnosis-tl-replace-entry "a" ["A" "λ"])
      (should (equal "A    λ\n" (buffer-string))))))

(ert-deftest gnosis-tl-replace-entry-is-surgical-and-preserves-state ()
  (with-temp-buffer
    (gnosis-test-tl--setup
     '(("a" ["Alice" "100"])
       ("b" ["Bob" "200"])
       ("c" ["Carol" "300"])))
    (goto-char (point-min))
    (forward-line 2)
    (set-buffer-modified-p nil)
    (let ((before-lines (count-lines (point-min) (point-max))))
      (gnosis-tl-replace-entry "b" ["Robert" "250"])
      (should (= before-lines (count-lines (point-min) (point-max))))
      (should (equal "c" (tabulated-list-get-id)))
      (should-not (buffer-modified-p))
      (goto-char (point-min))
      (should-not (search-forward "Bob" nil t))
      (goto-char (point-min))
      (should (search-forward "Robert" nil t)))))

(ert-deftest gnosis-tl-delete-entry-is-surgical-and-preserves-state ()
  (with-temp-buffer
    (gnosis-test-tl--setup
     '(("a" ["Alice" "100"])
       ("b" ["Bob" "200"])
       ("c" ["Carol" "300"])))
    (goto-char (point-min))
    (forward-line 2)
    (set-buffer-modified-p nil)
    (gnosis-tl-delete-entry "a")
    (should (equal "c" (tabulated-list-get-id)))
    (should (= 2 (count-lines (point-min) (point-max))))
    (should-not (buffer-modified-p))))

(ert-deftest gnosis-tl-append-entries-preserves-existing-rows ()
  (with-temp-buffer
    (gnosis-test-tl--setup '(("a" ["Alice" "100"])))
    (let ((prefix (buffer-string)))
      (set-buffer-modified-p nil)
      (gnosis-tl-append-entries
       '(("b" ["Bob" "200"])
         ("c" ["Carol" "300"])))
      (should (string-prefix-p prefix (buffer-string)))
      (should (= 3 (count-lines (point-min) (point-max))))
      (should-not (buffer-modified-p)))))

(ert-deftest gnosis-tl-bulk-render-preserves-column-navigation ()
  (dolist (append-row '(nil t))
    (with-temp-buffer
      (gnosis-test-tl--setup '(("a" ["Alice" "100"])))
      (when append-row
        (gnosis-tl-append-entries '(("b" ["Bob" "200"])))
        (forward-line 1))
      (move-to-column tabulated-list-padding)
      (should (equal "Name"
                     (get-text-property
                      (point) 'tabulated-list-column-name)))
      (tabulated-list-next-column 1)
      (should (equal "Value"
                     (get-text-property
                      (point) 'tabulated-list-column-name)))
      (tabulated-list-previous-column 1)
      (should (equal "Name"
                     (get-text-property
                      (point) 'tabulated-list-column-name))))))


(ert-deftest gnosis-tl-insert-entry-is-surgical-and-preserves-state ()
  (with-temp-buffer
    (gnosis-test-tl--setup
     '(("a" ["Alice" "100"])
       ("c" ["Carol" "300"])))
    (goto-char (point-min))
    (forward-line 1)
    (move-to-column 4)
    (set-buffer-modified-p nil)
    (let ((selected-column (current-column))
          (prefix (buffer-substring-no-properties
                   (point-min)
                   (progn (goto-char (point-min))
                          (forward-line 1)
                          (point)))))
      (goto-char (point-min))
      (forward-line 1)
      (move-to-column selected-column)
      (gnosis-tl-insert-entry "b" ["Bob" "200"] "c")
      (should (equal "c" (tabulated-list-get-id)))
      (should (= selected-column (current-column)))
      (should (= 3 (count-lines (point-min) (point-max))))
      (should (string-prefix-p prefix (buffer-string)))
      (goto-char (point-min))
      (forward-line 1)
      (should (equal "b" (tabulated-list-get-id)))
      (should-not (buffer-modified-p)))))

(provide 'gnosis-test-tl)
;;; gnosis-test-tl.el ends here
