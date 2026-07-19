;;; gnosis-completion.el --- Shared completion policy for Gnosis  -*- lexical-binding: t; -*-

;; Copyright (C) 2026  Free Software Foundation, Inc.

;; Author: Thanos Apollo <public@thanosapollo.org>
;; Keywords: extensions

;; This program is free software; you can redistribute it and/or modify
;; it under the terms of the GNU General Public License as published by
;; the Free Software Foundation, either version 3 of the License, or
;; (at your option) any later version.

;; This program is distributed in the hope that it will be useful,
;; but WITHOUT ANY WARRANTY; without even the implied warranty of
;; MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
;; GNU General Public License for more details.

;; You should have received a copy of the GNU General Public License
;; along with this program.  If not, see <https://www.gnu.org/licenses/>.

;;; Commentary:

;; Completion policy shared by focused Gnosis modules.

;;; Code:

(require 'cl-lib)

(defgroup gnosis nil
  "Emacs client for Gnosis."
  :group 'external
  :prefix "gnosis-")

(defcustom gnosis-completing-read-function
  (cond ((or (bound-and-true-p ivy-mode)
             (bound-and-true-p helm-mode)
             (bound-and-true-p vertico-mode)
             (bound-and-true-p fido-mode))
         #'completing-read)
        (t #'ido-completing-read))
  "Function used to read completion candidates.
It must accept PROMPT and COLLECTION.  When a match is required, it
also receives nil PREDICATE and non-nil REQUIRE-MATCH."
  :type 'function
  :group 'gnosis)

(defun gnosis-shuffle (sequence)
  "Return SEQUENCE shuffled in place."
  (cl-loop with length = (length sequence)
           for index from length downto 2
           do (cl-rotatef (nth (1- index) sequence)
                          (nth (random index) sequence))
           finally return sequence))

(defun gnosis-completing-read (prompt sequence &optional require-match)
  "Read a shuffled candidate from SEQUENCE using PROMPT.
When REQUIRE-MATCH is non-nil, require a member of SEQUENCE."
  (let ((history-add-new-input nil)
        (collection (gnosis-shuffle (copy-sequence sequence))))
    (if require-match
        (funcall gnosis-completing-read-function prompt collection nil t)
      (funcall gnosis-completing-read-function prompt collection))))

(provide 'gnosis-completion)
;;; gnosis-completion.el ends here
