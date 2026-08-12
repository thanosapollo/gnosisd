;;; gnosis-benchmark-ewoc-dashboard.el --- Deterministic EWOC scale proof -*- lexical-binding: t; -*-

;;; Commentary:

;; Exercise the real dashboard catalog callback and EWOC rendering path with
;; 50,001 records.  From this client directory, record complete evidence with:
;;
;;   nix develop --no-write-lock-file path:. --command env \
;;     GNOSIS_ENV_WRAPPED=1 time -v make benchmark-ewoc-dashboard

;;; Code:

(require 'cl-lib)
(require 'gnosis-dashboard)

(defconst gnosis-benchmark-ewoc-dashboard--total 50001)
(defconst gnosis-benchmark-ewoc-dashboard--page-size 100)

(defun gnosis-benchmark-ewoc-dashboard--record (index)
  "Return deterministic valid catalog record for INDEX."
  `(:thema_id ,(format "thema-%06d" index)
    :thema_revision 1 :state_revision 1 :review_count 0 :kind "basic"
    :keimenon ,(format "Question %d" index)
    :hypothesis [,gnosis-core--json-null] :answer ["Answer"] :tags []
    :archived ,gnosis-core--json-false
    :suspended ,gnosis-core--json-false))

(defun gnosis-benchmark-ewoc-dashboard--page (start count next)
  "Return deterministic catalog page from START with COUNT rows and NEXT."
  (let ((items (make-vector count nil)))
    (dotimes (offset count)
      (aset items offset
            (gnosis-benchmark-ewoc-dashboard--record (+ start offset))))
    `(:generation "benchmark-generation" :items ,items :next ,next)))

(defun gnosis-benchmark-ewoc-dashboard--projection ()
  "Return deterministic dashboard summary projection."
  '(:generated_at_us 1 :review_day 1
    :current_streak_days 1
    :today (:review_count 1 :new_count 1 :success_count 1
            :failure_count 0 :override_count 0)
    :window (:from_day 1 :through_day 1 :days 30 :active_days 1
             :totals (:review_count 1 :new_count 1 :success_count 1
                      :failure_count 0 :override_count 0)
             :average_per_calendar_day 0.03 :average_per_active_day 1.0
             :success_rate 1.0)
    :queue (:new_count 1 :due_count 1 :overdue_count 0 :suspended_count 0)
    :content (:active_themata 1 :archived_themata 0
              :active_documents 1 :active_nodes 1)
    :active_session gnosis-core--json-null))

(defun gnosis-benchmark-ewoc-dashboard-run ()
  "Run deterministic 50,001-row EWOC benchmark and print evidence."
  (let ((started (float-time))
        (next-index 0)
        (catalog-calls 0)
        (callback-calls 0)
        (pretty-calls 0)
        (max-pretty-calls 0)
        terminal-cursor
        jobs)
    (cl-letf (((symbol-function 'gnosis-core-dashboard)
               #'gnosis-benchmark-ewoc-dashboard--projection)
              ((symbol-function 'gnosis-core-thema-catalog)
               (lambda (&rest _)
                 (cl-incf catalog-calls)
                 (let* ((remaining (- gnosis-benchmark-ewoc-dashboard--total
                                      next-index))
                        (count (min gnosis-benchmark-ewoc-dashboard--page-size
                                    remaining))
                        (start next-index)
                        (next (if (> remaining count)
                                  (format "thema-%06d"
                                          (+ next-index count -1))
                                gnosis-core--json-null)))
                   (cl-incf next-index count)
                   (setq terminal-cursor next)
                   (gnosis-benchmark-ewoc-dashboard--page start count next))))
              ((symbol-function 'gnosis-core--token)
               (lambda () (make-string 40 ?a)))
              ((symbol-function 'gnosis-core--validated-base-url)
               (lambda () "https://benchmark.example"))
              ((symbol-function 'gnosis-core--authenticated-authority)
               (lambda (&optional token)
                 (vector "https://benchmark.example"
                         (secure-hash 'sha256
                                      (or token (make-string 40 ?a))))))
              ((symbol-function 'run-at-time)
               (lambda (_time _repeat function &rest arguments)
                 (setq jobs (append jobs (list (cons function arguments))))
                 'timer))
              ((symbol-function 'keymap-popup) #'ignore)
              ((symbol-function 'pop-to-buffer-same-window)
               (lambda (buffer &rest _) (set-buffer buffer)))
              ((symbol-function 'gnosis-themata--pretty-print)
               (lambda (record)
                 (cl-incf pretty-calls)
                 (gnosis-themata--pretty-print-default record))))
      (when-let* ((buffer (get-buffer gnosis-dashboard-themata-buffer-name)))
        (kill-buffer buffer))
      (unwind-protect
          (progn
            (gnosis-dashboard-themata)
            (while jobs
              (setq pretty-calls 0)
              (let ((job (pop jobs)))
                (cl-incf callback-calls)
                (apply (car job) (cdr job)))
              (setq max-pretty-calls (max max-pretty-calls pretty-calls)))
            (with-current-buffer gnosis-dashboard-themata-buffer-name
              (let* ((records (ewoc-collect gnosis-themata--ewoc #'identity))
                     (rows (length records))
                     (last-id (plist-get (car (last records)) :thema_id))
                     (rendered-chars (buffer-size))
                     (elapsed (- (float-time) started)))
                (unless (and (= rows gnosis-benchmark-ewoc-dashboard--total)
                             (= rows (hash-table-count gnosis-themata--nodes))
                             (= next-index gnosis-benchmark-ewoc-dashboard--total)
                             (eq terminal-cursor gnosis-core--json-null)
                             (equal last-id "thema-050000")
                             (null gnosis-themata--refresh-state)
                             (not gnosis-themata--loading-p)
                             (<= max-pretty-calls
                                 gnosis-benchmark-ewoc-dashboard--page-size))
                  (error "EWOC benchmark invariant failed"))
                (princ
                 (format
                  (concat "rows=%d terminal_cursor=null catalog_calls=%d "
                          "callback_calls=%d max_rows_per_callback=%d "
                          "rendered_chars=%d elapsed_seconds=%.3f\n")
                  rows catalog-calls callback-calls max-pretty-calls
                  rendered-chars elapsed)))))
        (when-let* ((buffer (get-buffer gnosis-dashboard-themata-buffer-name)))
          (kill-buffer buffer))))))

(gnosis-benchmark-ewoc-dashboard-run)

;;; gnosis-benchmark-ewoc-dashboard.el ends here
